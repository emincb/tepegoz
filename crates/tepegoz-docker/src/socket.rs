//! Platform docker socket discovery.
//!
//! The list is *intentionally* macOS-leaning at the top because that's where
//! the runtime ambiguity lives: a single Mac may have Docker Desktop installed
//! but not running, plus Colima or Rancher Desktop. On native Linux the list
//! collapses to the single canonical `/var/run/docker.sock`.
//!
//! Order matters — first reachable candidate wins. If a user has Colima
//! running alongside Docker Desktop, they almost certainly want Colima
//! (Docker Desktop being installed-but-stopped is the common case).

use std::path::PathBuf;

/// One candidate the discovery walk will probe. Held as a struct so the
/// engine can report a human-readable label alongside the raw path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocketCandidate {
    /// Short human-friendly source name, e.g. "Docker Desktop", "Colima".
    pub label: &'static str,
    /// Concrete path on disk (`exists()` is checked before connecting).
    pub path: PathBuf,
}

/// Ordered list of socket candidates to probe. Caller is expected to walk
/// in order and stop at the first one that pings successfully.
#[must_use]
pub fn discover_socket_candidates() -> Vec<SocketCandidate> {
    candidate_paths_with_home(std::env::var_os("HOME").map(PathBuf::from))
}

#[doc(hidden)]
pub fn candidate_paths_for_test(home: PathBuf) -> Vec<SocketCandidate> {
    candidate_paths_with_home(Some(home))
}

fn candidate_paths_with_home(home: Option<PathBuf>) -> Vec<SocketCandidate> {
    let mut out = Vec::with_capacity(5);

    if let Some(home) = home.as_ref() {
        // Docker Desktop on macOS — moved out of /var/run/docker.sock years
        // back; the symlink still exists but the real socket lives here.
        out.push(SocketCandidate {
            label: "Docker Desktop",
            path: home.join(".docker").join("run").join("docker.sock"),
        });
        // Colima — default profile.
        out.push(SocketCandidate {
            label: "Colima (default)",
            path: home.join(".colima").join("default").join("docker.sock"),
        });
        // Rancher Desktop.
        out.push(SocketCandidate {
            label: "Rancher Desktop",
            path: home.join(".rd").join("docker.sock"),
        });
    }

    // Native Linux daemon — also the symlink target on macOS Docker Desktop,
    // so it covers the older Mac install layout too. Last because we prefer
    // running rootful daemons over a possibly-dangling system symlink.
    out.push(SocketCandidate {
        label: "system",
        path: PathBuf::from("/var/run/docker.sock"),
    });

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_includes_user_and_system_paths() {
        let home = PathBuf::from("/home/test");
        let candidates = candidate_paths_with_home(Some(home.clone()));

        let labels: Vec<&str> = candidates.iter().map(|c| c.label).collect();
        assert_eq!(
            labels,
            vec![
                "Docker Desktop",
                "Colima (default)",
                "Rancher Desktop",
                "system"
            ],
            "expected discovery order: Docker Desktop, Colima, Rancher, system. \
             This order matters — first reachable wins."
        );

        assert_eq!(
            candidates[0].path,
            home.join(".docker/run/docker.sock"),
            "Docker Desktop socket path must be ~/.docker/run/docker.sock"
        );
        assert_eq!(
            candidates[3].path,
            PathBuf::from("/var/run/docker.sock"),
            "system socket path must be /var/run/docker.sock"
        );
    }

    #[test]
    fn discovery_without_home_still_returns_system() {
        let candidates = candidate_paths_with_home(None);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].label, "system");
    }
}
