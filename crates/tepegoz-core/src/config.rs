//! Daemon configuration.

use std::path::PathBuf;

/// Function that resolves a remote target triple
/// (`"x86_64-unknown-linux-musl"` / `"aarch64-apple-darwin"` / …) to
/// the bytes of the controller's compile-time-embedded `tepegoz-agent`
/// binary for that target.
///
/// Returning `None` means "not embedded at build time" — the Fleet
/// supervisor logs a warning and continues without an agent for that
/// host (remote Docker / Ports / Processes subscriptions against the
/// alias will surface `DockerUnavailable { reason: "agent not …" }`).
///
/// Shape is a bare `fn` pointer (not a `Box<dyn Fn>`) so the
/// compile-time `embedded_agents::for_target` function pointer from
/// the controller `build.rs` is a direct fit, with no lifetime or
/// Send/Sync gymnastics.
pub type AgentResolver = fn(&str) -> Option<&'static [u8]>;

pub struct DaemonConfig {
    /// Override the default Unix socket path.
    pub socket_path: Option<PathBuf>,
}
