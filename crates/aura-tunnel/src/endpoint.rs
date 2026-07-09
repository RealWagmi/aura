//! The symmetric tunnel endpoint: one Noise_NNpsk0 session over a connected
//! UDP socket, with an inbound jitter buffer and an outbound 20 ms pacer. Used
//! by BOTH sides — the server wraps it as an `AudioTransport` (`transport.rs`),
//! the thin client drives it with cpal.
//!
//! Flow once the handshake completes:
//! - **outbound** (`send_pcm24`): reframe to exact 480-sample (20 ms) frames →
//!   queue; a pacer task ticks every 20 ms, encrypts one frame under a fresh
//!   per-packet nonce, and sends it. `send_pcm24` never blocks on the network;
//!   `clear_playout` drains the queue (barge-in).
//! - **inbound**: an I/O task reads datagrams, decrypts by the carried nonce,
//!   and pushes into the jitter buffer; on a 20 ms tick it pops one in-order
//!   frame to a channel that `recv_pcm24` awaits. A lost packet is a dropped
//!   20 ms (PLC silence), never a stall. Control frames ride the SAME jitter
//!   buffer so they cannot overtake the audio sent before them (a `PttClose`
//!   that outruns the turn's trailing frames clips the user's final word and
//!   leaks it into the next turn).

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{interval, timeout, MissedTickBehavior};

use crate::jitter::JitterBuffer;
use crate::noise::{self, Transport, MAX_HANDSHAKE_MSG};
use crate::outbound::{bytes_to_pcm, Outbound};
use crate::session::SessionSecret;
use crate::wire::{
    decode_transport, decode_tunnel_control, encode_transport, TunnelControl, TunnelInput,
    TAG_HANDSHAKE, TAG_TRANSPORT,
};

/// 20 ms @ 24 kHz mono.
const FRAME_SAMPLES: usize = 480;
const FRAME_MS: u64 = 20;
/// Receive buffer: a transport datagram is ~985 bytes (960 PCM + nonce + tag).
const RECV_BUF: usize = 2048;

/// Tunnel timing/sizing knobs.
#[derive(Debug, Clone, Copy)]
pub struct TunnelConfig {
    /// How long the server waits for the (single) client handshake, and how
    /// long the client retries before giving up. Bounds the secret's life.
    pub handshake_timeout: Duration,
    /// Client handshake-message retransmit interval over lossy UDP.
    pub handshake_retransmit: Duration,
    /// Inbound frame channel capacity.
    pub inbound_capacity: usize,
    /// Tear the call down if no valid datagram (audio OR keepalive) arrives
    /// within this window — surfaces a silently-vanished UDP peer (crash / NAT
    /// expiry / partition) as `recv_pcm24() -> None`. The 20 ms keepalives keep
    /// a genuinely idle-but-alive peer well under it.
    pub idle_timeout: Duration,
}

impl Default for TunnelConfig {
    fn default() -> Self {
        Self {
            handshake_timeout: Duration::from_secs(120),
            handshake_retransmit: Duration::from_millis(250),
            inbound_capacity: 64,
            idle_timeout: Duration::from_secs(8),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TunnelError {
    #[error("tunnel io: {0}")]
    Io(#[from] std::io::Error),
    #[error("tunnel noise: {0}")]
    Noise(#[from] noise::NoiseError),
    #[error("handshake timed out")]
    HandshakeTimeout,
}

/// Aborts the spawned task when the endpoint is dropped (no lingering UDP
/// loops, no leaked sockets).
struct AbortOnDrop(JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A bound server UDP socket waiting to accept one tunnel handshake. Split from
/// `accept` so the caller can read [`Self::local_addr`] (for the connection
/// string) before blocking on the client.
pub struct TunnelServer {
    socket: UdpSocket,
}

impl TunnelServer {
    /// Bind the server UDP socket (use `:0` for an ephemeral port).
    pub async fn bind(addr: &str) -> Result<Self, TunnelError> {
        Ok(Self {
            socket: UdpSocket::bind(addr).await?,
        })
    }

    /// The actually-bound local address (resolve the ephemeral port here).
    pub fn local_addr(&self) -> Result<SocketAddr, TunnelError> {
        Ok(self.socket.local_addr()?)
    }

    /// Wait (≤ `handshake_timeout`) for the client's handshake, complete it as
    /// the Noise responder (PSK = `secret`), and return the live endpoint.
    /// Datagrams authenticated under the wrong PSK are ignored (the legitimate
    /// client succeeds; the timeout bounds an attacker's spam).
    pub async fn accept(
        self,
        secret: &SessionSecret,
        cfg: TunnelConfig,
    ) -> Result<TunnelEndpoint, TunnelError> {
        let socket = self.socket;
        let mut buf = [0u8; RECV_BUF];
        let mut scratch = [0u8; MAX_HANDSHAKE_MSG];
        let deadline = tokio::time::Instant::now() + cfg.handshake_timeout;
        // A genuine msg1 datagram is exactly this size; reject anything else
        // BEFORE building an RNG-seeded responder (anti-amplification).
        let expected = 1 + noise::msg1_len();

        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(TunnelError::HandshakeTimeout);
            }
            let (n, peer) = match timeout(remaining, socket.recv_from(&mut buf)).await {
                Ok(r) => r?,
                Err(_) => return Err(TunnelError::HandshakeTimeout),
            };
            if n != expected || buf[0] != TAG_HANDSHAKE {
                continue; // not a plausibly-sized handshake msg1 — cheap reject
            }
            let mut hs = noise::responder(secret.as_bytes())?;
            if hs.read_message(&buf[1..n], &mut scratch).is_err() {
                continue; // wrong PSK / malformed — keep waiting for the real client
            }
            let n2 = match hs.write_message(&[], &mut scratch) {
                Ok(n) => n,
                Err(_) => continue,
            };
            let mut msg2 = Vec::with_capacity(1 + n2);
            msg2.push(TAG_HANDSHAKE);
            msg2.extend_from_slice(&scratch[..n2]);
            socket.send_to(&msg2, peer).await?;
            socket.connect(peer).await?;
            let transport = Arc::new(noise::finalize(hs)?);
            return Ok(TunnelEndpoint::spawn(
                Arc::new(socket),
                transport,
                cfg,
                Some(msg2),
            ));
        }
    }
}

/// A live tunnel endpoint: PCM16@24k in/out over the encrypted UDP session.
pub struct TunnelEndpoint {
    outbound: Arc<Mutex<Outbound>>,
    inbound_rx: mpsc::Receiver<TunnelInput>,
    _send_task: AbortOnDrop,
    _io_task: AbortOnDrop,
}

impl TunnelEndpoint {
    /// Dial `server_addr`, complete the Noise handshake as the initiator
    /// (PSK = `secret`), retransmitting msg1 over lossy UDP until msg2 arrives
    /// or `handshake_timeout` elapses.
    pub async fn connect_client(
        server_addr: &str,
        secret: &SessionSecret,
        cfg: TunnelConfig,
    ) -> Result<Self, TunnelError> {
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        socket.connect(server_addr).await?;

        let mut hs = noise::initiator(secret.as_bytes())?;
        let mut scratch = [0u8; MAX_HANDSHAKE_MSG];
        let n1 = hs
            .write_message(&[], &mut scratch)
            .map_err(noise::NoiseError::from)?;
        let mut msg1 = Vec::with_capacity(1 + n1);
        msg1.push(TAG_HANDSHAKE);
        msg1.extend_from_slice(&scratch[..n1]);

        let mut buf = [0u8; RECV_BUF];
        let deadline = tokio::time::Instant::now() + cfg.handshake_timeout;
        loop {
            if tokio::time::Instant::now() >= deadline {
                return Err(TunnelError::HandshakeTimeout);
            }
            socket.send(&msg1).await?;
            match timeout(cfg.handshake_retransmit, socket.recv(&mut buf)).await {
                Ok(Ok(n)) if n >= 1 && buf[0] == TAG_HANDSHAKE => {
                    hs.read_message(&buf[1..n], &mut scratch)
                        .map_err(noise::NoiseError::from)?;
                    break;
                }
                Ok(Ok(_)) => continue, // unexpected datagram — retransmit
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => continue, // retransmit timeout
            }
        }
        let transport = Arc::new(noise::finalize(hs)?);
        Ok(Self::spawn(Arc::new(socket), transport, cfg, None))
    }

    /// Wire up the send pacer + I/O task over an established session. The socket
    /// is already `connect`ed to the peer. `handshake_retx` (server side) is the
    /// msg2 datagram to resend if a duplicate handshake arrives (client's msg2
    /// was lost).
    fn spawn(
        socket: Arc<UdpSocket>,
        transport: Arc<Transport>,
        cfg: TunnelConfig,
        handshake_retx: Option<Vec<u8>>,
    ) -> Self {
        let outbound = Arc::new(Mutex::new(Outbound::new(FRAME_SAMPLES)));
        let (inbound_tx, inbound_rx) = mpsc::channel::<TunnelInput>(cfg.inbound_capacity.max(1));

        // Outbound pacer: every 20 ms send one queued frame, OR — when the queue
        // is idle — an encrypted empty "keepalive" frame. The steady keepalive
        // refreshes the peer's liveness timer and keeps NAT mappings warm; it
        // never blocks the engine (`send_pcm24` only enqueues).
        let send_task = {
            let socket = socket.clone();
            let transport = transport.clone();
            let outbound = outbound.clone();
            tokio::spawn(async move {
                let mut tick = interval(Duration::from_millis(FRAME_MS));
                tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
                let mut nonce: u64 = 0;
                loop {
                    tick.tick().await;
                    // Empty plaintext == keepalive (still AEAD-authenticated, so
                    // a spoofer without the key can't forge liveness).
                    let bytes = outbound
                        .lock()
                        .expect("outbound lock")
                        .pop_next()
                        .unwrap_or_default();
                    match transport.encrypt(nonce, &bytes) {
                        Ok(ct) => {
                            let dg = encode_transport(nonce, &ct);
                            nonce = nonce.wrapping_add(1);
                            if socket.send(&dg).await.is_err() {
                                break; // peer gone
                            }
                        }
                        Err(_) => break,
                    }
                }
            })
        };

        // Inbound I/O: read+decrypt into the jitter buffer; pop one in-order
        // frame per 20 ms tick to the channel `recv_pcm24` awaits. The same tick
        // enforces the idle/liveness deadline: a silently-vanished UDP peer
        // (which yields no `recv` error) is torn down here.
        let io_task = {
            let socket = socket.clone();
            let idle_timeout = cfg.idle_timeout;
            tokio::spawn(async move {
                let mut jitter = JitterBuffer::new();
                let mut buf = [0u8; RECV_BUF];
                let mut tick = interval(Duration::from_millis(FRAME_MS));
                tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
                let mut last_recv = tokio::time::Instant::now();
                loop {
                    tokio::select! {
                        r = socket.recv(&mut buf) => match r {
                            Ok(n) if n >= 1 && buf[0] == TAG_TRANSPORT => {
                                if let Some((nonce, ct)) = decode_transport(&buf[1..n]) {
                                    if let Ok(pt) = transport.decrypt(nonce, ct) {
                                        // Any authenticated frame proves liveness;
                                        // empty == keepalive (not pushed to audio).
                                        // Control frames go through the SAME jitter
                                        // buffer as audio: release order == send
                                        // order, and the control's sequence number
                                        // no longer reads as a lost audio packet
                                        // (which stalled the turn's trailing frames
                                        // past the commit).
                                        last_recv = tokio::time::Instant::now();
                                        if !pt.is_empty() {
                                            jitter.push(nonce as u16, pt);
                                        }
                                    }
                                }
                            }
                            Ok(n) if n >= 1 && buf[0] == TAG_HANDSHAKE => {
                                // Peer retransmitted a handshake (its msg2 was
                                // lost); resend ours if we are the responder.
                                if let Some(dg) = &handshake_retx {
                                    let _ = socket.send(dg).await;
                                }
                            }
                            Ok(_) => {}
                            Err(_) => break, // socket closed → drop inbound_tx → recv_pcm24 None
                        },
                        _ = tick.tick() => {
                            if last_recv.elapsed() > idle_timeout {
                                break; // peer vanished with no close → hang up
                            }
                            if let Some(bytes) = jitter.pop() {
                                if let Some(control) = decode_tunnel_control(&bytes) {
                                    // A control is a state transition, not a
                                    // droppable sample: await channel space
                                    // (a lost PttClose strands the turn).
                                    if inbound_tx.send(TunnelInput::Control(control)).await.is_err() {
                                        break;
                                    }
                                } else if inbound_tx.try_send(TunnelInput::Audio(bytes_to_pcm(&bytes))).is_err()
                                    && inbound_tx.is_closed()
                                {
                                    // Drop this frame if the consumer is far behind
                                    // (channel full); a closed channel ends the task.
                                    break;
                                }
                            }
                        }
                    }
                }
            })
        };

        Self {
            outbound,
            inbound_rx,
            _send_task: AbortOnDrop(send_task),
            _io_task: AbortOnDrop(io_task),
        }
    }

    /// Queue model/mic audio for sending, reframed to exact 20 ms frames. The
    /// queue is bounded (drop-oldest audio; controls survive) so a stalled/dead
    /// pacer can't grow memory.
    pub fn send_pcm24(&self, pcm: &[i16]) {
        self.outbound.lock().expect("outbound lock").push_pcm(pcm);
    }

    /// Flush the `<20 ms` reframer tail (padded with silence) so a phrase
    /// ending isn't held back.
    pub fn flush_output(&self) {
        self.outbound.lock().expect("outbound lock").flush_tail();
    }

    /// Queue an authenticated control event for the peer, in-order with the
    /// audio queued before it.
    pub fn send_control(&self, control: TunnelControl) {
        self.outbound
            .lock()
            .expect("outbound lock")
            .push_control(control);
    }

    /// The next inbound audio/control event, or `None` when the tunnel closes.
    pub async fn recv_input(&mut self) -> Option<TunnelInput> {
        self.inbound_rx.recv().await
    }

    /// The next inbound 20 ms frame, or `None` when the tunnel closes.
    pub async fn recv_pcm24(&mut self) -> Option<Vec<i16>> {
        loop {
            match self.recv_input().await? {
                TunnelInput::Audio(pcm) => return Some(pcm),
                TunnelInput::Control(_) => continue,
            }
        }
    }

    /// Drop the queued AUDIO and reset the reframer carry (barge-in must not
    /// leave a stale partial frame to prepend onto the next response). Queued
    /// control frames are preserved.
    pub fn clear_outbound(&self) {
        self.outbound.lock().expect("outbound lock").clear_audio();
    }

    /// Milliseconds of audio queued for sending.
    pub fn outbound_queued_ms(&self) -> u64 {
        self.outbound.lock().expect("outbound lock").queued_ms()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn loopback_tunnel_handshakes_and_round_trips_pcm() {
        let secret = SessionSecret::generate().unwrap();
        let server = TunnelServer::bind("127.0.0.1:0").await.unwrap();
        let addr = server.local_addr().unwrap().to_string();

        let server_secret = secret.clone();
        let server_handle = tokio::spawn(async move {
            server
                .accept(&server_secret, TunnelConfig::default())
                .await
                .unwrap()
        });

        let mut client = TunnelEndpoint::connect_client(&addr, &secret, TunnelConfig::default())
            .await
            .unwrap();
        let mut server_ep = server_handle.await.unwrap();

        // Feed enough frames to fill the jitter pre-buffer, both directions.
        for i in 0..8 {
            client.send_pcm24(&[100 + i as i16; FRAME_SAMPLES]);
            server_ep.send_pcm24(&[200 + i as i16; FRAME_SAMPLES]);
        }

        let got_server = timeout(Duration::from_secs(3), server_ep.recv_pcm24())
            .await
            .expect("server recv timed out")
            .expect("server tunnel closed");
        assert_eq!(got_server.len(), FRAME_SAMPLES);
        assert_eq!(got_server[0], 100, "first client frame");

        let got_client = timeout(Duration::from_secs(3), client.recv_pcm24())
            .await
            .expect("client recv timed out")
            .expect("client tunnel closed");
        assert_eq!(got_client[0], 200, "first server frame");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn control_frame_does_not_overtake_audio_sent_before_it() {
        let secret = SessionSecret::generate().unwrap();
        let server = TunnelServer::bind("127.0.0.1:0").await.unwrap();
        let addr = server.local_addr().unwrap().to_string();

        let server_secret = secret.clone();
        let server_handle = tokio::spawn(async move {
            server
                .accept(&server_secret, TunnelConfig::default())
                .await
                .unwrap()
        });
        let client = TunnelEndpoint::connect_client(&addr, &secret, TunnelConfig::default())
            .await
            .unwrap();
        let mut server_ep = server_handle.await.unwrap();

        // A push-to-talk turn: audio frames, then the PttClose that commits
        // them. The server must observe ALL the audio before the control —
        // over the old direct-delivery path the control overtook the jitter
        // buffer and clipped the turn's trailing ~40-80 ms.
        const TURN_FRAMES: usize = 8;
        for i in 0..TURN_FRAMES {
            client.send_pcm24(&[i as i16 + 1; FRAME_SAMPLES]);
        }
        client.send_control(TunnelControl::PttClose);

        let mut audio_seen = 0usize;
        loop {
            let input = timeout(Duration::from_secs(3), server_ep.recv_input())
                .await
                .expect("server recv timed out")
                .expect("server tunnel closed");
            match input {
                TunnelInput::Audio(pcm) => {
                    audio_seen += 1;
                    assert_eq!(pcm[0] as usize, audio_seen, "audio released in order");
                }
                TunnelInput::Control(c) => {
                    assert_eq!(c, TunnelControl::PttClose);
                    break;
                }
            }
        }
        assert_eq!(
            audio_seen, TURN_FRAMES,
            "every audio frame of the turn must arrive before its PttClose"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wrong_secret_client_cannot_complete_handshake() {
        let server_secret = SessionSecret::generate().unwrap();
        let server = TunnelServer::bind("127.0.0.1:0").await.unwrap();
        let addr = server.local_addr().unwrap().to_string();
        let server_handle = tokio::spawn(async move {
            server
                .accept(
                    &server_secret,
                    TunnelConfig {
                        handshake_timeout: Duration::from_millis(600),
                        ..Default::default()
                    },
                )
                .await
        });
        // Client with a DIFFERENT secret: the responder ignores its msg1, so the
        // client retransmits until its own timeout, and the server times out.
        let wrong = SessionSecret::generate().unwrap();
        let client = TunnelEndpoint::connect_client(
            &addr,
            &wrong,
            TunnelConfig {
                handshake_timeout: Duration::from_millis(600),
                ..Default::default()
            },
        )
        .await;
        assert!(client.is_err(), "wrong-secret client must not connect");
        assert!(
            server_handle.await.unwrap().is_err(),
            "server must time out"
        );
    }
}
