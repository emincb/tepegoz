//! Tepegöz wire protocol.
//!
//! Every transport (Unix socket, SSH channel, QUIC, WSS) carries the same
//! envelope on the wire:
//!
//! ```text
//! [4-byte big-endian length] [rkyv-encoded Envelope]
//! ```
//!
//! Compat policy: `Envelope` carries an explicit `version`. Peers reject
//! unknown versions and use generated migration handlers between compatible
//! versions. Validation via `bytecheck` is mandatory on the network boundary
//! (agent/remote) and optional on the trusted local Unix socket for perf.

/// Current wire protocol version. Bumps on breaking change.
pub const PROTOCOL_VERSION: u32 = 1;
