//! iroh QUIC transport backend — the OPTIONAL second REMOTE transport, for
//! servers behind NAT/CGNAT (a home PC with no public IP / no port-forward).
//! Selected by `AURA_TRANSPORT=iroh`; gated behind the `iroh` cargo feature so
//! the default build is untouched.
//!
//! It is a faithful analog of [`crate::endpoint::TunnelEndpoint`]: the SAME
//! Noise_NNpsk0 session, [`Reframer`], [`JitterBuffer`] and 20 ms pacer. Only
//! the "socket" changes:
//! - **audio** rides iroh **QUIC datagrams** (unreliable, unordered — exactly
//!   like our UDP path, so the per-packet-nonce framing + jitter buffer apply
//!   verbatim; no head-of-line blocking on loss);
//! - the **Noise handshake** runs over a reliable QUIC **bi-stream** (so no
//!   UDP-style msg retransmit is needed).
//!
//! iroh provides hole-punching plus a **blind, encrypted relay fallback** (it
//! only ever sees QUIC ciphertext), so the call connects without an open port.
//! Our `Noise_NNpsk0` runs INSIDE the iroh connection: iroh authenticates the
//! peer's endpoint key and moves bytes through NAT; the per-call PSK authorises
//! *this* call. Security model unchanged from the direct transport.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iroh::endpoint::{presets, Connection};
use iroh::{Endpoint, EndpointAddr, EndpointId, SecretKey, TransportAddr};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{interval, timeout, MissedTickBehavior};

use crate::endpoint::TunnelConfig;
use crate::jitter::JitterBuffer;
use crate::noise::{self, Transport, MAX_HANDSHAKE_MSG};
use crate::reframe::Reframer;
use crate::session::SessionSecret;
use crate::wire::{
    decode_transport, decode_tunnel_control, encode_transport, encode_tunnel_control,
    TunnelControl, TunnelInput, TAG_TRANSPORT,
};

/// ALPN identifying the aura voice tunnel over iroh. Both sides must match, or
/// iroh aborts the connection in its handshake.
pub const ALPN: &[u8] = b"aura/voice/0";

/// 20 ms @ 24 kHz mono.
const FRAME_SAMPLES: usize = 480;
const FRAME_MS: u64 = 20;
/// Outbound queue cap — a MEMORY backstop only (matches the direct transport).
/// The model bursts a full answer faster than realtime, so the queue must hold
/// a whole LONG answer or drop-oldest audibly eats words (live-diagnosed on the
/// direct transport at the old 30 s cap; see `endpoint.rs`). 5 min @ 50
/// frames/s = 15 000 frames ≈ 14 MB; overflow is logged loudly.
const MAX_OUTBOUND_FRAMES: usize = 15_000;

/// Which iroh preset (relay + discovery) to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrohPreset {
    /// Production: n0 relay servers + DNS/pkarr discovery — holepunch through
    /// NAT/CGNAT, blind relay fallback.
    Production,
    /// Local/direct only: NO relay, NO discovery. For loopback/LAN where the
    /// peer is reached by an explicit direct address (offline-testable).
    LocalDirect,
}

#[derive(Debug, thiserror::Error)]
pub enum IrohError {
    #[error("iroh: {0}")]
    Iroh(String),
    #[error("iroh noise: {0}")]
    Noise(#[from] noise::NoiseError),
    #[error("iroh handshake: {0}")]
    Handshake(String),
    #[error("iroh handshake timed out")]
    HandshakeTimeout,
}

/// Map any `Display` error (iroh's `n0_error`, io, quinn) into [`IrohError`].
fn ierr<E: std::fmt::Display>(e: E) -> IrohError {
    IrohError::Iroh(e.to_string())
}

fn pcm_to_bytes(pcm: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pcm.len() * 2);
    for s in pcm {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

fn bytes_to_pcm(bytes: &[u8]) -> Vec<i16> {
    bytes
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect()
}

/// Generate an ephemeral iroh endpoint identity (one per call — the per-call
/// `EndpointId` is what the connection string carries; matches the single-use
/// secret model).
fn ephemeral_secret_key() -> Result<SecretKey, IrohError> {
    let mut kb = [0u8; 32];
    getrandom::getrandom(&mut kb).map_err(|e| IrohError::Iroh(e.to_string()))?;
    Ok(SecretKey::from_bytes(&kb))
}

/// Build an iroh endpoint for `preset`. The accept side passes the ALPN; the
/// connect side passes `None` (it presents the ALPN on `connect`).
async fn build_endpoint(
    preset: IrohPreset,
    secret_key: SecretKey,
    accept_alpn: Option<&[u8]>,
) -> Result<Endpoint, IrohError> {
    let builder = match preset {
        IrohPreset::Production => Endpoint::builder(presets::N0),
        // `Minimal` sets only the mandatory rustls crypto provider (ring) — no
        // relay, no discovery. We additionally pin relay OFF and bind loopback
        // so a direct-only call needs no network at all.
        IrohPreset::LocalDirect => Endpoint::builder(presets::Minimal),
    };
    let mut builder = builder.secret_key(secret_key);
    if let Some(alpn) = accept_alpn {
        builder = builder.alpns(vec![alpn.to_vec()]);
    }
    if preset == IrohPreset::LocalDirect {
        builder = builder
            .relay_mode(iroh::RelayMode::Disabled)
            .bind_addr(
                "127.0.0.1:0"
                    .parse::<std::net::SocketAddr>()
                    .expect("const addr"),
            )
            .map_err(ierr)?;
    }
    let endpoint = builder.bind().await.map_err(ierr)?;
    // `online()` waits for a relay home; with relay disabled (LocalDirect) it
    // would never resolve. Only the Production path needs to wait for relay/
    // discovery registration before its address is dialable.
    if preset == IrohPreset::Production {
        endpoint.online().await;
    }
    Ok(endpoint)
}

/// Length-prefixed write of one Noise handshake message over the bi-stream.
async fn write_framed<W: AsyncWriteExt + Unpin>(w: &mut W, msg: &[u8]) -> Result<(), IrohError> {
    let len = u16::try_from(msg.len())
        .map_err(|_| IrohError::Handshake("handshake message too large".to_owned()))?;
    w.write_all(&len.to_be_bytes()).await.map_err(ierr)?;
    w.write_all(msg).await.map_err(ierr)?;
    w.flush().await.map_err(ierr)?;
    Ok(())
}

/// Length-prefixed read of one Noise handshake message from the bi-stream.
async fn read_framed<R: AsyncReadExt + Unpin>(r: &mut R) -> Result<Vec<u8>, IrohError> {
    let mut len_buf = [0u8; 2];
    r.read_exact(&mut len_buf).await.map_err(ierr)?;
    let len = u16::from_be_bytes(len_buf) as usize;
    if len > MAX_HANDSHAKE_MSG {
        return Err(IrohError::Handshake(
            "handshake message too large".to_owned(),
        ));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await.map_err(ierr)?;
    Ok(buf)
}

/// Outbound state behind one lock (same shape as the UDP path): the pacer queue
/// and the reframer. `clear_outbound` resets both so a stale partial frame can't
/// prepend onto the next response (barge-in).
struct Outbound {
    queue: VecDeque<Vec<u8>>,
    reframer: Reframer,
    /// Total frames dropped on overflow (diagnostic; see `MAX_OUTBOUND_FRAMES`).
    dropped_frames: u64,
}

/// Aborts the spawned task when the endpoint is dropped.
struct AbortOnDrop(JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A bound iroh endpoint waiting to accept one tunnel call. Split from `accept`
/// so the caller can read [`Self::endpoint_id`] / [`Self::addr`] (for the
/// connection string) before blocking on the client.
pub struct IrohServer {
    endpoint: Endpoint,
    preset: IrohPreset,
}

impl IrohServer {
    /// Bind an iroh endpoint that will accept one aura call.
    pub async fn bind(preset: IrohPreset) -> Result<Self, IrohError> {
        let endpoint = build_endpoint(preset, ephemeral_secret_key()?, Some(ALPN)).await?;
        Ok(Self { endpoint, preset })
    }

    /// This server's endpoint id — the public key the client dials. Goes into
    /// the connection string (`aura://<EndpointId>#…&t=iroh`).
    pub fn endpoint_id(&self) -> EndpointId {
        self.endpoint.id()
    }

    /// The dialable address for the connection string. Production uses iroh's
    /// own resolved address (relay + direct, after `online()`); LocalDirect
    /// builds a direct-only address from the bound loopback socket(s), which is
    /// known immediately without relay/discovery/netcheck.
    pub fn addr(&self) -> EndpointAddr {
        match self.preset {
            IrohPreset::Production => self.endpoint.addr(),
            IrohPreset::LocalDirect => {
                let addrs = self
                    .endpoint
                    .bound_sockets()
                    .into_iter()
                    .map(TransportAddr::Ip);
                EndpointAddr::from_parts(self.endpoint.id(), addrs)
            }
        }
    }

    /// Wait (≤ `handshake_timeout`) for the client's iroh connection, complete
    /// the Noise responder handshake over its bi-stream (PSK = `secret`), and
    /// return the live endpoint. A client without the PSK fails the Noise read.
    pub async fn accept(
        self,
        secret: &SessionSecret,
        cfg: TunnelConfig,
    ) -> Result<IrohEndpoint, IrohError> {
        // Bound the WHOLE accept (connection + Noise handshake) so a client that
        // connects but stalls mid-handshake can't strand the server.
        let (conn, transport) = timeout(cfg.handshake_timeout, async {
            let incoming =
                self.endpoint.accept().await.ok_or_else(|| {
                    IrohError::Iroh("endpoint closed before a connection".to_owned())
                })?;
            let conn = incoming.await.map_err(ierr)?;
            // Responder Noise handshake over the bi-stream the client opens.
            let (mut send, mut recv) = conn.accept_bi().await.map_err(ierr)?;
            let mut hs = noise::responder(secret.as_bytes())?;
            let mut scratch = [0u8; MAX_HANDSHAKE_MSG];
            let msg1 = read_framed(&mut recv).await?;
            hs.read_message(&msg1, &mut scratch)
                .map_err(|_| IrohError::Handshake("bad msg1 / wrong session secret".to_owned()))?;
            let n2 = hs
                .write_message(&[], &mut scratch)
                .map_err(noise::NoiseError::from)?;
            write_framed(&mut send, &scratch[..n2]).await?;
            let _ = send.finish();
            Ok::<_, IrohError>((conn, Arc::new(noise::finalize(hs)?)))
        })
        .await
        .map_err(|_| IrohError::HandshakeTimeout)??;

        Ok(IrohEndpoint::spawn(
            self.endpoint,
            Arc::new(conn),
            transport,
            cfg,
        ))
    }
}

/// A live iroh tunnel endpoint: PCM16@24k in/out over the encrypted QUIC
/// session. Same surface as [`crate::endpoint::TunnelEndpoint`].
pub struct IrohEndpoint {
    // Kept alive so the underlying connection survives; dropping it closes the call.
    _endpoint: Endpoint,
    outbound: Arc<Mutex<Outbound>>,
    inbound_rx: mpsc::Receiver<TunnelInput>,
    _send_task: AbortOnDrop,
    _io_task: AbortOnDrop,
}

impl IrohEndpoint {
    /// Connect to a server `addr` (its [`EndpointAddr`] from the connection
    /// string), completing the Noise initiator handshake (PSK = `secret`) over a
    /// bi-stream. `preset` must match the server's reachability mode.
    pub async fn connect_client(
        addr: EndpointAddr,
        secret: &SessionSecret,
        preset: IrohPreset,
        cfg: TunnelConfig,
    ) -> Result<Self, IrohError> {
        let endpoint = build_endpoint(preset, ephemeral_secret_key()?, None).await?;
        // Bound the WHOLE connect (QUIC connection + Noise handshake) so a server
        // that accepts but never answers msg2 can't strand the client.
        let (conn, transport) = timeout(cfg.handshake_timeout, async {
            let conn = endpoint.connect(addr, ALPN).await.map_err(ierr)?;
            // Initiator Noise handshake over a fresh bi-stream.
            let (mut send, mut recv) = conn.open_bi().await.map_err(ierr)?;
            let mut hs = noise::initiator(secret.as_bytes())?;
            let mut scratch = [0u8; MAX_HANDSHAKE_MSG];
            let n1 = hs
                .write_message(&[], &mut scratch)
                .map_err(noise::NoiseError::from)?;
            write_framed(&mut send, &scratch[..n1]).await?;
            let msg2 = read_framed(&mut recv).await?;
            hs.read_message(&msg2, &mut scratch)
                .map_err(noise::NoiseError::from)?;
            let _ = send.finish();
            Ok::<_, IrohError>((conn, Arc::new(noise::finalize(hs)?)))
        })
        .await
        .map_err(|_| IrohError::HandshakeTimeout)??;

        Ok(Self::spawn(endpoint, Arc::new(conn), transport, cfg))
    }

    /// Connect to a server by its `EndpointId` string (the connection string's
    /// authority when `t=iroh`), resolving the server's address via iroh
    /// discovery. Keeps the thin client free of iroh types.
    pub async fn connect_by_id(
        endpoint_id: &str,
        secret: &SessionSecret,
        preset: IrohPreset,
        cfg: TunnelConfig,
    ) -> Result<Self, IrohError> {
        let id: EndpointId = endpoint_id
            .parse()
            .map_err(|_| IrohError::Iroh(format!("invalid endpoint id: {endpoint_id}")))?;
        Self::connect_client(EndpointAddr::from(id), secret, preset, cfg).await
    }

    /// Wire up the 20 ms send pacer + the inbound I/O task over an established
    /// session (mirrors `TunnelEndpoint::spawn`, but over iroh datagrams).
    fn spawn(
        endpoint: Endpoint,
        conn: Arc<Connection>,
        transport: Arc<Transport>,
        cfg: TunnelConfig,
    ) -> Self {
        let outbound = Arc::new(Mutex::new(Outbound {
            queue: VecDeque::new(),
            reframer: Reframer::new(FRAME_SAMPLES),
            dropped_frames: 0,
        }));
        let (inbound_tx, inbound_rx) = mpsc::channel::<TunnelInput>(cfg.inbound_capacity.max(1));

        // Outbound pacer: every 20 ms send one queued frame as a QUIC datagram,
        // OR (idle) an encrypted empty keepalive — keeps QUIC + any relay/NAT
        // mapping warm and never blocks the engine (`send_pcm24` only enqueues).
        let send_task = {
            let conn = conn.clone();
            let transport = transport.clone();
            let outbound = outbound.clone();
            tokio::spawn(async move {
                let mut tick = interval(Duration::from_millis(FRAME_MS));
                tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
                let mut nonce: u64 = 0;
                loop {
                    tick.tick().await;
                    let bytes = outbound
                        .lock()
                        .expect("outbound lock")
                        .queue
                        .pop_front()
                        .unwrap_or_default();
                    match transport.encrypt(nonce, &bytes) {
                        Ok(ct) => {
                            let dg = encode_transport(nonce, &ct);
                            nonce = nonce.wrapping_add(1);
                            if conn.send_datagram(dg.into()).is_err() {
                                break; // connection gone
                            }
                        }
                        Err(_) => break,
                    }
                }
            })
        };

        // Inbound I/O: read+decrypt datagrams into the jitter buffer; pop one
        // in-order frame per 20 ms tick to the channel `recv_pcm24` awaits. A
        // closed connection (peer gone / hang-up) ends the task → `recv` None.
        let io_task = {
            let conn = conn.clone();
            tokio::spawn(async move {
                let mut jitter = JitterBuffer::new();
                let mut tick = interval(Duration::from_millis(FRAME_MS));
                tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
                loop {
                    tokio::select! {
                        r = conn.read_datagram() => match r {
                            Ok(msg) => {
                                let body: &[u8] = &msg;
                                if body.first() == Some(&TAG_TRANSPORT) {
                                    if let Some((nonce, ct)) = decode_transport(&body[1..]) {
                                        if let Ok(pt) = transport.decrypt(nonce, ct) {
                                            // Empty == keepalive (authenticated); not audio.
                                            if let Some(control) = decode_tunnel_control(&pt) {
                                                if inbound_tx.try_send(TunnelInput::Control(control)).is_err()
                                                    && inbound_tx.is_closed()
                                                {
                                                    break;
                                                }
                                            } else if !pt.is_empty() {
                                                jitter.push(nonce as u16, pt);
                                            }
                                        }
                                    }
                                }
                            }
                            Err(_) => break, // connection closed → drop inbound_tx → recv None
                        },
                        _ = tick.tick() => {
                            if let Some(bytes) = jitter.pop() {
                                if inbound_tx.try_send(TunnelInput::Audio(bytes_to_pcm(&bytes))).is_err()
                                    && inbound_tx.is_closed()
                                {
                                    break;
                                }
                            }
                        }
                    }
                }
            })
        };

        Self {
            _endpoint: endpoint,
            outbound,
            inbound_rx,
            _send_task: AbortOnDrop(send_task),
            _io_task: AbortOnDrop(io_task),
        }
    }

    /// Queue model/mic audio for sending, reframed to exact 20 ms frames.
    pub fn send_pcm24(&self, pcm: &[i16]) {
        let mut out = self.outbound.lock().expect("outbound lock");
        let frames = out.reframer.push(pcm);
        for f in frames {
            out.queue.push_back(pcm_to_bytes(&f));
            while out.queue.len() > MAX_OUTBOUND_FRAMES {
                out.queue.pop_front();
                out.dropped_frames += 1;
                // Loud, rate-limited (first drop, then ~1/s of loss): overflow
                // eats the audio the listener is ABOUT TO HEAR.
                if out.dropped_frames == 1 || out.dropped_frames.is_multiple_of(50) {
                    eprintln!(
                        "aura-tunnel: outbound pacer queue FULL ({} min cap) — {} ms of audio \
                         dropped; words are being skipped",
                        MAX_OUTBOUND_FRAMES as u64 / 50 / 60,
                        out.dropped_frames * FRAME_MS
                    );
                }
            }
        }
    }

    /// Flush the `<20 ms` reframer tail (padded with silence) so a phrase ending
    /// isn't held back.
    pub fn flush_output(&self) {
        let mut out = self.outbound.lock().expect("outbound lock");
        if let Some(mut tail) = out.reframer.flush() {
            tail.resize(FRAME_SAMPLES, 0);
            out.queue.push_back(pcm_to_bytes(&tail));
        }
    }

    /// Queue an authenticated control event for the peer.
    pub fn send_control(&self, control: TunnelControl) {
        self.outbound
            .lock()
            .expect("outbound lock")
            .queue
            .push_back(encode_tunnel_control(control));
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

    /// Drop everything queued for sending AND reset the reframer carry (barge-in).
    pub fn clear_outbound(&self) {
        let mut out = self.outbound.lock().expect("outbound lock");
        out.queue.clear();
        out.reframer = Reframer::new(FRAME_SAMPLES);
    }

    /// Milliseconds of audio queued for sending.
    pub fn outbound_queued_ms(&self) -> u64 {
        self.outbound.lock().expect("outbound lock").queue.len() as u64 * FRAME_MS
    }
}

/// The server-side [`aura_engine::AudioTransport`] over an [`IrohEndpoint`] —
/// the iroh counterpart of `TunnelTransport`. Needs the `server` feature (for
/// the `aura-engine` dep). The engine sees the same 24 kHz PCM seam regardless
/// of which transport carries it.
#[cfg(feature = "server")]
pub struct IrohTransport {
    endpoint: IrohEndpoint,
}

#[cfg(feature = "server")]
impl IrohTransport {
    pub fn new(endpoint: IrohEndpoint) -> Self {
        Self { endpoint }
    }
}

#[cfg(feature = "server")]
#[async_trait::async_trait]
impl aura_engine::AudioTransport for IrohTransport {
    async fn recv_pcm24(&mut self) -> Option<Vec<i16>> {
        self.endpoint.recv_pcm24().await
    }

    async fn recv_input(&mut self) -> Option<aura_engine::TransportInput> {
        self.endpoint.recv_input().await.map(|input| match input {
            TunnelInput::Audio(pcm) => aura_engine::TransportInput::Audio(pcm),
            TunnelInput::Control(TunnelControl::PttOpen) => {
                aura_engine::TransportInput::Control(aura_engine::TransportControl::PttOpen)
            }
            TunnelInput::Control(TunnelControl::PttClose) => {
                aura_engine::TransportInput::Control(aura_engine::TransportControl::PttClose)
            }
        })
    }

    async fn send_pcm24(&mut self, pcm: &[i16]) -> Result<(), aura_engine::TransportError> {
        // Enqueue for the 20 ms pacer; never blocks. A dead peer surfaces as
        // `recv_pcm24() -> None`, the engine's hang-up signal.
        self.endpoint.send_pcm24(pcm);
        Ok(())
    }

    fn clear_playout(&self) {
        self.endpoint.clear_outbound();
    }

    fn queued_ms(&self) -> u64 {
        self.endpoint.outbound_queued_ms()
    }

    async fn flush_output(&mut self) -> Result<(), aura_engine::TransportError> {
        self.endpoint.flush_output();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn loopback_iroh_handshakes_and_round_trips_pcm() {
        let secret = SessionSecret::generate().unwrap();
        // Fail fast on any hang rather than the 120 s default.
        let cfg = TunnelConfig {
            handshake_timeout: Duration::from_secs(10),
            ..Default::default()
        };
        let server = IrohServer::bind(IrohPreset::LocalDirect).await.unwrap();
        let server_addr = server.addr();

        let server_secret = secret.clone();
        let server_handle =
            tokio::spawn(async move { server.accept(&server_secret, cfg).await.unwrap() });

        let mut client =
            IrohEndpoint::connect_client(server_addr, &secret, IrohPreset::LocalDirect, cfg)
                .await
                .unwrap();
        let mut server_ep = server_handle.await.unwrap();

        for i in 0..8 {
            client.send_pcm24(&[100 + i as i16; FRAME_SAMPLES]);
            server_ep.send_pcm24(&[200 + i as i16; FRAME_SAMPLES]);
        }

        let got_server = timeout(Duration::from_secs(5), server_ep.recv_pcm24())
            .await
            .expect("server recv timed out")
            .expect("server tunnel closed");
        assert_eq!(got_server.len(), FRAME_SAMPLES);
        assert_eq!(got_server[0], 100, "first client frame");

        let got_client = timeout(Duration::from_secs(5), client.recv_pcm24())
            .await
            .expect("client recv timed out")
            .expect("client tunnel closed");
        assert_eq!(got_client[0], 200, "first server frame");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wrong_secret_client_cannot_complete_handshake() {
        let server_secret = SessionSecret::generate().unwrap();
        let server = IrohServer::bind(IrohPreset::LocalDirect).await.unwrap();
        let server_addr = server.addr();
        let cfg = TunnelConfig {
            handshake_timeout: Duration::from_millis(1500),
            ..Default::default()
        };
        let server_handle = tokio::spawn(async move { server.accept(&server_secret, cfg).await });

        let wrong = SessionSecret::generate().unwrap();
        let client =
            IrohEndpoint::connect_client(server_addr, &wrong, IrohPreset::LocalDirect, cfg).await;

        // The responder rejects msg1 under the wrong PSK → server errors; the
        // client's handshake read fails or its connect is reset.
        assert!(
            server_handle.await.unwrap().is_err(),
            "server must reject the wrong-secret client"
        );
        assert!(client.is_err(), "wrong-secret client must not connect");
    }
}
