//! `TunnelTransport` — the server-side [`aura_engine::AudioTransport`] over a
//! [`TunnelEndpoint`]. Behind the `server` feature so the thin client need not
//! depend on `aura-engine`.
//!
//! The engine sees the same 24 kHz PCM seam as everywhere else: inbound frames
//! are the client's mic; outbound frames are the model's audio, paced at 20 ms
//! and dropped on barge-in (`clear_playout`).

use aura_engine::{AudioTransport, TransportControl, TransportError, TransportInput};

use crate::endpoint::TunnelEndpoint;
use crate::wire::{TunnelControl, TunnelInput};

/// Wraps a [`TunnelEndpoint`] as the engine's audio transport.
pub struct TunnelTransport {
    endpoint: TunnelEndpoint,
}

impl TunnelTransport {
    pub fn new(endpoint: TunnelEndpoint) -> Self {
        Self { endpoint }
    }
}

#[async_trait::async_trait]
impl AudioTransport for TunnelTransport {
    async fn recv_pcm24(&mut self) -> Option<Vec<i16>> {
        self.endpoint.recv_pcm24().await
    }

    async fn recv_input(&mut self) -> Option<TransportInput> {
        self.endpoint.recv_input().await.map(|input| match input {
            TunnelInput::Audio(pcm) => TransportInput::Audio(pcm),
            TunnelInput::Control(TunnelControl::PttOpen) => {
                TransportInput::Control(TransportControl::PttOpen)
            }
            TunnelInput::Control(TunnelControl::PttClose) => {
                TransportInput::Control(TransportControl::PttClose)
            }
            TunnelInput::Control(TunnelControl::PttCancel) => {
                TransportInput::Control(TransportControl::PttCancel)
            }
        })
    }

    async fn send_pcm24(&mut self, pcm: &[i16]) -> Result<(), TransportError> {
        // Enqueue for the 20 ms pacer; never blocks on the network. A dead peer
        // surfaces as `recv_pcm24() -> None`, the engine's hang-up signal.
        self.endpoint.send_pcm24(pcm);
        Ok(())
    }

    fn clear_playout(&self) {
        self.endpoint.clear_outbound();
    }

    fn queued_ms(&self) -> u64 {
        self.endpoint.outbound_queued_ms()
    }

    async fn flush_output(&mut self) -> Result<(), TransportError> {
        self.endpoint.flush_output();
        Ok(())
    }
}
