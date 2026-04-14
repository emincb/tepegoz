//! Cross-platform probes for processes, sockets, filesystem, and system stats.
//!
//! Phase 4 Slice 4a shipped the Ports probe: listening TCP sockets with
//! owning pid + process name + cgroup-correlated container id on Linux.
//! macOS port-to-container correlation completes in the daemon layer
//! (pid → container requires a Docker engine lookup since macOS pids are
//! host-VM, not in-container). Slice 4b adds the Processes probe:
//! sysinfo-backed, cross-OS, stateful across samples so CPU% delta
//! computation works.
//!
//! Platform-specific helpers live under [`linux`] / [`macos`]; the
//! `common` module is reserved for sysinfo-based fallbacks if a platform
//! grows one.

pub mod common;
pub mod ports;
pub mod processes;

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(target_os = "macos")]
pub mod macos;

pub use ports::{PortsError, SOURCE_LABEL, list_ports};
pub use processes::{ProcessesError, ProcessesProbe};
