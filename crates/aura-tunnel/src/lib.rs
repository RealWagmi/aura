//! `aura-tunnel` — the REMOTE transport: a dedicated encrypted client↔server
//! tunnel.
//!
//! The voice channel is a **Noise_NNpsk0** session (the per-call session secret
//! IS the PSK → mutual auth + forward secrecy + anti-MITM, no certs/domain/PKI)
//! carried over **UDP** (realtime; no TCP head-of-line blocking), with an
//! adaptive jitter buffer + 20 ms pacing. By default raw PCM16 mono @ 24 kHz
//! flows over the tunnel; an optional codec feature adds Opus.
//!
//! Layout:
//! - [`jitter`] — adaptive jitter buffer + 20 ms send pacing (transport-agnostic).
//! - [`reframe`] — `Reframer{carry}`: exact-frame reassembly, needed only when
//!   the optional Opus codec is enabled.
//! - `noise`/`wire`/`session`/`transport`/`client` — the Noise/UDP tunnel.

pub mod endpoint;
pub mod jitter;
pub mod noise;
pub mod reframe;
pub mod session;
pub mod wire;

#[cfg(feature = "server")]
pub mod transport;

/// Optional second REMOTE transport: iroh QUIC P2P for NAT/CGNAT servers.
#[cfg(feature = "iroh")]
pub mod iroh_transport;

pub use endpoint::{TunnelConfig, TunnelEndpoint, TunnelError, TunnelServer};
#[cfg(all(feature = "iroh", feature = "server"))]
pub use iroh_transport::IrohTransport;
#[cfg(feature = "iroh")]
pub use iroh_transport::{IrohEndpoint, IrohError, IrohPreset, IrohServer};
pub use jitter::JitterBuffer;
pub use reframe::Reframer;
pub use session::SessionSecret;
#[cfg(feature = "server")]
pub use transport::TunnelTransport;
pub use wire::{ConnectionString, TransportKind};
