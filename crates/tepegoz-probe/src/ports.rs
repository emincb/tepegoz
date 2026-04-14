//! Listening-port probe facade — picks the right OS-native implementation at
//! compile time and returns `ProbePort` rows ready for the wire.
//!
//! Phase 4 Slice 4a covers TCP listeners only. UDP support and `Processes`
//! enumeration land in later slices (4b+).

use netstat2::{AddressFamilyFlags, ProtocolFlags, ProtocolSocketInfo, TcpState, get_sockets_info};
use sysinfo::{ProcessRefreshKind, RefreshKind, System};
use tepegoz_proto::ProbePort;

/// Non-fatal errors from the ports probe.
///
/// The daemon's forward task treats any error here as a `PortsUnavailable`
/// event — it keeps retrying at its own cadence and the TUI shows the
/// reason. No need for a kind-level enum yet; the string is what the user
/// will read.
#[derive(Debug, thiserror::Error)]
pub enum PortsError {
    /// Backend failed (netstat2 failed to open /proc or call libproc).
    /// Most common on Linux when /proc/net/tcp* is inaccessible.
    #[error("ports probe failed: {0}")]
    Backend(String),

    /// Current platform is neither Linux nor macOS.
    #[error("ports probe is unsupported on this platform ({0})")]
    Unsupported(&'static str),
}

/// Human-readable identifier for the current probe implementation. Delivered
/// in `Event::PortList { source, .. }` so the TUI can surface it in the tile
/// footer (mirrors `engine_source` on Docker's ContainerList).
#[cfg(target_os = "linux")]
pub const SOURCE_LABEL: &str = "linux-procfs";
#[cfg(target_os = "macos")]
pub const SOURCE_LABEL: &str = "macos-libproc";
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub const SOURCE_LABEL: &str = "unsupported";

/// Enumerate listening TCP sockets as [`ProbePort`] rows.
///
/// Behavior:
/// - IPv4 + IPv6 listeners are returned separately (many services bind only
///   one family, some bind both — don't silently dedupe).
/// - Each socket's owning pid comes from the native backend (netstat2
///   inodes on Linux, libproc pidfdinfo on macOS). `pid == 0` with
///   `partial: true` means the probe saw the socket but couldn't attribute
///   it — usually a privilege issue.
/// - Process names come from `sysinfo`'s process table, looked up by pid.
/// - On Linux, `container_id` is filled from `/proc/<pid>/cgroup` when the
///   owning process runs under a docker scope. On macOS `container_id` is
///   always `None` — the daemon layer completes the correlation by
///   cross-referencing port numbers against Docker's bollard container list
///   (macOS pids are Docker Desktop VM host pids, not in-container pids).
pub fn list_ports() -> Result<Vec<ProbePort>, PortsError> {
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        return Err(PortsError::Unsupported(std::env::consts::OS));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let af_flags = AddressFamilyFlags::IPV4 | AddressFamilyFlags::IPV6;
        let proto_flags = ProtocolFlags::TCP;
        let sockets = get_sockets_info(af_flags, proto_flags)
            .map_err(|e| PortsError::Backend(e.to_string()))?;

        // Build pid → name map once per poll. sysinfo's `everything()` preset
        // is overkill; we only need the short process name, which
        // `ProcessRefreshKind::new()` already includes.
        let system = System::new_with_specifics(
            RefreshKind::new().with_processes(ProcessRefreshKind::new()),
        );

        let mut out = Vec::with_capacity(sockets.len());
        for info in sockets {
            let ProtocolSocketInfo::Tcp(tcp) = info.protocol_socket_info else {
                continue;
            };
            if tcp.state != TcpState::Listen {
                continue;
            }
            out.push(build_row(&tcp, &info.associated_pids, &system));
        }
        Ok(out)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn build_row(tcp: &netstat2::TcpSocketInfo, associated_pids: &[u32], system: &System) -> ProbePort {
    let pid = associated_pids.first().copied().unwrap_or(0);
    let (process_name, container_id, partial_details) = if pid == 0 {
        (String::new(), None, true)
    } else {
        let name = system
            .process(sysinfo::Pid::from_u32(pid))
            .map(|p| p.name().to_string_lossy().to_string())
            .unwrap_or_default();
        let container_id = container_id_for_pid(pid);
        let partial = name.is_empty();
        (name, container_id, partial)
    };

    ProbePort {
        local_ip: tcp.local_addr.to_string(),
        local_port: tcp.local_port,
        protocol: "tcp".into(),
        pid,
        process_name,
        container_id,
        partial: pid == 0 || partial_details,
    }
}

/// Linux: read `/proc/<pid>/cgroup` and extract a docker container id if
/// present. macOS returns `None` (pid → container correlation happens in
/// the daemon layer).
#[cfg(target_os = "linux")]
fn container_id_for_pid(pid: u32) -> Option<String> {
    crate::linux::container_id_for_pid(pid)
}

#[cfg(not(target_os = "linux"))]
#[allow(dead_code)]
fn container_id_for_pid(_pid: u32) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_label_matches_platform() {
        #[cfg(target_os = "linux")]
        assert_eq!(SOURCE_LABEL, "linux-procfs");
        #[cfg(target_os = "macos")]
        assert_eq!(SOURCE_LABEL, "macos-libproc");
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        assert_eq!(SOURCE_LABEL, "unsupported");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn list_ports_returns_a_vec_without_panicking() {
        // We can't assert on specific entries — the test host might have any
        // mix of listeners — but the call must at least succeed and return a
        // Vec (possibly empty if nothing listens; possibly populated if the
        // test runner itself has opened diagnostic ports).
        let ports = list_ports().expect("list_ports must not error on a supported OS");
        for p in &ports {
            assert!(
                p.local_port > 0,
                "listening port zero indicates a broken probe row: {p:?}"
            );
            assert_eq!(
                p.protocol, "tcp",
                "Slice 4a only emits TCP listeners: {p:?}"
            );
        }
    }
}
