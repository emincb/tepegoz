//! Cross-platform probes for processes, sockets, filesystem, and system stats.
//!
//! Platform-specific backends live under [`linux`] and [`macos`]. A
//! `sysinfo`-backed fallback lives under [`common`].

pub mod common;

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(target_os = "macos")]
pub mod macos;
