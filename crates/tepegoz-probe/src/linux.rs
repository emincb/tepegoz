//! Linux-native helpers.
//!
//! Phase 4 Slice 4a ships a single helper: `container_id_for_pid`, which
//! reads `/proc/<pid>/cgroup` and extracts a docker container id if the
//! process lives under a docker scope. Used by the Ports probe to correlate
//! listening sockets to containers without needing a Docker engine
//! connection (cgroup is a filesystem source of truth). The socket
//! enumeration itself goes through the cross-OS `netstat2` wrapper.

use std::fs;

/// Look up the docker container id that owns `pid`, or `None` if the process
/// is not under a docker scope (or `/proc/<pid>/cgroup` is unreadable).
pub fn container_id_for_pid(pid: u32) -> Option<String> {
    let content = fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    content.lines().find_map(|line| {
        // cgroup v1 line: "12:cpu,cpuacct:/docker/abc123def456"
        // cgroup v2 line: "0::/system.slice/docker-abc123def456.scope"
        // The path segment lives after the last `:` — grab it, then scan
        // for a `docker`-prefixed hex run.
        let path = line.rsplit_once(':').map(|(_, p)| p).unwrap_or(line);
        container_id_from_cgroup_path(path)
    })
}

/// Extract a docker container id from a cgroup path segment. Handles:
///   - cgroup v1 direct: `/docker/<id>`
///   - cgroup v1 systemd: `/system.slice/docker-<id>.scope`
///   - cgroup v2: `/system.slice/docker-<id>.scope`
///   - kubelet-nested: `/kubepods/.../docker-<id>.scope`
///
/// The id is 12–64 hex chars (short or full form). We accept either since
/// clients treat it as opaque. Returns `None` for non-docker paths.
pub(crate) fn container_id_from_cgroup_path(path: &str) -> Option<String> {
    // Find the last `docker` occurrence, skip any `-` / `/` separator, then
    // scan the ascii-hex run that follows.
    let tail = path.rsplit_once("docker")?.1;
    let tail = tail.trim_start_matches(|c: char| c == '-' || c == '/');
    let id: String = tail.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
    if id.len() >= 12 { Some(id) } else { None }
}

#[cfg(test)]
mod tests {
    use super::container_id_from_cgroup_path;

    #[test]
    fn cgroup_v1_direct_docker_path() {
        assert_eq!(
            container_id_from_cgroup_path("/docker/abc123def4567890"),
            Some("abc123def4567890".to_string())
        );
    }

    #[test]
    fn cgroup_v2_systemd_docker_scope_full_id() {
        let full = "a".repeat(64);
        let path = format!("/system.slice/docker-{full}.scope");
        assert_eq!(container_id_from_cgroup_path(&path), Some(full));
    }

    #[test]
    fn cgroup_v1_systemd_docker_scope_short_id() {
        assert_eq!(
            container_id_from_cgroup_path("/system.slice/docker-abc123def456.scope"),
            Some("abc123def456".to_string())
        );
    }

    #[test]
    fn cgroup_kubelet_nested_docker_scope() {
        assert_eq!(
            container_id_from_cgroup_path(
                "/kubepods/besteffort/pod123abc/docker-feedbeefcafe1234.scope"
            ),
            Some("feedbeefcafe1234".to_string())
        );
    }

    #[test]
    fn cgroup_non_docker_user_slice_returns_none() {
        assert_eq!(
            container_id_from_cgroup_path("/user.slice/user-1000.slice"),
            None
        );
    }

    #[test]
    fn cgroup_empty_path_returns_none() {
        assert_eq!(container_id_from_cgroup_path(""), None);
    }

    #[test]
    fn cgroup_short_id_below_12_chars_rejected() {
        // Even though the path has `docker`, the trailing run is too short
        // to be a real docker id.
        assert_eq!(container_id_from_cgroup_path("/docker/abc"), None);
    }

    #[test]
    fn cgroup_non_hex_after_docker_returns_none() {
        // "zebra" starts with a non-hex character so the take_while yields ""
        // — length 0 is below the 12-char floor → None.
        assert_eq!(container_id_from_cgroup_path("/docker/zebra123"), None);
    }

    #[test]
    fn cgroup_containerd_path_with_docker_substring() {
        // Some runtimes embed "docker" in an unrelated segment. Our rule is
        // "take the last `docker` occurrence + trailing hex run", which
        // matches `docker-<hex>.scope` even if earlier segments mention it.
        assert_eq!(
            container_id_from_cgroup_path("/docker-unrelated/containerd/docker-1234567890ab.scope"),
            Some("1234567890ab".to_string())
        );
    }
}
