//! Cross-platform probes for processes, sockets, filesystem, and system stats.
//!
//! Phase 4 Slice 4a ships the Ports probe: listening TCP sockets with owning
//! pid + process name + cgroup-correlated container id on Linux. macOS
//! correlation completes in the daemon layer (pid → container requires a
//! Docker engine lookup since macOS pids are host-VM, not in-container).
//! Processes enumeration and UDP are follow-up slices.
//!
//! Platform-specific helpers live under [`linux`] / [`macos`]; a `sysinfo`
//! fallback lives under [`common`] (reserved for Slice 4b).

pub mod common;
pub mod ports;

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(target_os = "macos")]
pub mod macos;

pub use ports::{PortsError, SOURCE_LABEL, list_ports};
