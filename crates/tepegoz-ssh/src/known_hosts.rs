//! Host-key TOFU against a tepegoz-owned known_hosts file.
//!
//! Location: `data_dir/tepegoz/known_hosts` — separate from
//! `~/.ssh/known_hosts` so tepegöz never mutates the user's primary
//! OpenSSH state. The file format is OpenSSH-compatible (russh's
//! `check_known_hosts_path` / `learn_known_hosts_path` helpers do the
//! read/write), so users can inspect or hand-edit with standard tools.
//!
//! Semantics: on `Unknown` we auto-accept and persist (classic TOFU);
//! on `Mismatch` we reject and surface
//! [`SshError::HostKeyMismatch`](crate::error::SshError::HostKeyMismatch)
//! with the stored line number. The user's recovery path is the
//! planned `tepegoz doctor --ssh-forget <alias>` command (ships in 5b
//! alongside `--ssh-hosts`).

use std::path::{Path, PathBuf};

use russh::keys::PublicKey;
use russh::keys::known_hosts;

use crate::error::SshError;
use crate::paths;

/// Verdict for a presented server key against the local TOFU store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostKeyVerdict {
    /// A stored entry matches the presented key.
    Trusted,
    /// No entry for `(hostname, port)`; caller should trust-and-persist.
    Unknown,
    /// A stored entry disagrees with the presented key — reject.
    Mismatch { stored_line: usize },
}

/// Thin handle over a tepegoz-owned known_hosts file. Cheap to clone:
/// just a PathBuf. The `check` / `trust` ops read and append the file
/// each call (matches OpenSSH's own approach — no in-memory cache).
#[derive(Debug, Clone)]
pub struct KnownHostsStore {
    path: PathBuf,
}

impl KnownHostsStore {
    /// Open the known_hosts file at the standard platform path. Returns
    /// `Err(SshError::PathResolution)` on headless environments where
    /// `data_dir()` can't be resolved.
    pub fn open() -> Result<Self, SshError> {
        Ok(Self {
            path: paths::require_known_hosts_path()?,
        })
    }

    /// Open at an explicit path — used by tests and any future
    /// `--known-hosts` override.
    pub fn open_at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Check a presented public key against any entry for
    /// `(hostname, port)`.
    pub fn check(
        &self,
        hostname: &str,
        port: u16,
        key: &PublicKey,
    ) -> Result<HostKeyVerdict, SshError> {
        match known_hosts::check_known_hosts_path(hostname, port, key, &self.path) {
            Ok(true) => Ok(HostKeyVerdict::Trusted),
            Ok(false) => Ok(HostKeyVerdict::Unknown),
            Err(russh::keys::Error::KeyChanged { line }) => {
                Ok(HostKeyVerdict::Mismatch { stored_line: line })
            }
            Err(e) => Err(SshError::KnownHosts {
                path: self.path.clone(),
                reason: e.to_string(),
            }),
        }
    }

    /// Append a new entry. Creates the parent directory if missing.
    /// Called on TOFU auto-accept for first-contact hosts.
    ///
    /// After russh writes the entry, we set the file mode to `0600` on
    /// Unix to match OpenSSH's convention for `~/.ssh/known_hosts`.
    /// The file contains server public keys (not sensitive by
    /// themselves) but matching OpenSSH's posture removes one "why is
    /// this world-readable?" surprise. Russh's `learn_known_hosts_path`
    /// does not set a mode explicitly — verified 2026-04-14 against
    /// russh 0.60.
    pub fn trust(&self, hostname: &str, port: u16, key: &PublicKey) -> Result<(), SshError> {
        known_hosts::learn_known_hosts_path(hostname, port, key, &self.path).map_err(|e| {
            SshError::KnownHosts {
                path: self.path.clone(),
                reason: e.to_string(),
            }
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            if let Err(e) = std::fs::set_permissions(&self.path, perms) {
                // Don't fail the operation — the entry is persisted;
                // mode is a defense-in-depth nicety. Trace-level log.
                tracing::debug!(
                    path = %self.path.display(),
                    error = %e,
                    "failed to chmod 0600 on known_hosts (entry was written successfully)"
                );
            }
        }
        Ok(())
    }
}

// ------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // Valid ed25519 public-key base64 payloads — matches russh's own
    // known_hosts test vectors. Parsed via `parse_public_key_base64`
    // (no OpenSSH wrapper / comment) so round-trip byte comparisons
    // match what TOFU stores + reads back.
    const KEY_A: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ";
    const KEY_B: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIA6rWI3G1sz07DnfFlrouTcysQlj2P+jpNSOEWD9OJ3X";

    fn parse(base64: &str) -> PublicKey {
        russh::keys::parse_public_key_base64(base64).unwrap()
    }

    #[test]
    fn unknown_host_without_entry_returns_unknown() {
        let dir = TempDir::new().unwrap();
        let store = KnownHostsStore::open_at(dir.path().join("known_hosts"));
        let verdict = store.check("nonexistent.box", 22, &parse(KEY_A)).unwrap();
        assert_eq!(verdict, HostKeyVerdict::Unknown);
    }

    #[test]
    fn trust_then_check_returns_trusted() {
        let dir = TempDir::new().unwrap();
        let store = KnownHostsStore::open_at(dir.path().join("known_hosts"));
        let key = parse(KEY_A);
        store.trust("my.box", 22, &key).unwrap();
        let verdict = store.check("my.box", 22, &key).unwrap();
        assert_eq!(verdict, HostKeyVerdict::Trusted);
    }

    #[test]
    fn presenting_different_key_returns_mismatch_with_line() {
        let dir = TempDir::new().unwrap();
        let store = KnownHostsStore::open_at(dir.path().join("known_hosts"));
        store.trust("my.box", 22, &parse(KEY_A)).unwrap();
        let verdict = store.check("my.box", 22, &parse(KEY_B)).unwrap();
        match verdict {
            HostKeyVerdict::Mismatch { stored_line } => {
                // russh's `learn_known_hosts_path` writes a leading `\n`
                // on an empty file so line numbering is 1-based starting
                // after the leading newline — the first real entry is
                // at line 2. Pin the invariant loosely: line > 0.
                assert!(
                    stored_line > 0,
                    "stored_line should be a 1-based line number, got {stored_line}"
                );
            }
            other => panic!("expected Mismatch, got {other:?}"),
        }
    }

    #[test]
    fn non_standard_port_encodes_with_brackets() {
        // OpenSSH format: `[host]:port` for non-22 ports. Verify TOFU
        // round-trips correctly under that encoding.
        let dir = TempDir::new().unwrap();
        let store = KnownHostsStore::open_at(dir.path().join("known_hosts"));
        let key = parse(KEY_A);
        store.trust("alt.box", 2222, &key).unwrap();
        assert_eq!(
            store.check("alt.box", 2222, &key).unwrap(),
            HostKeyVerdict::Trusted
        );
        // Same host on port 22 is a different entry — should not match.
        assert_eq!(
            store.check("alt.box", 22, &key).unwrap(),
            HostKeyVerdict::Unknown
        );
    }

    #[cfg(unix)]
    #[test]
    fn trust_sets_known_hosts_mode_to_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("known_hosts");
        let store = KnownHostsStore::open_at(&path);
        store.trust("chmod.box", 22, &parse(KEY_A)).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "known_hosts should be 0600 after trust (OpenSSH convention)"
        );
    }

    #[test]
    fn trust_creates_missing_parent_directory() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("a").join("b").join("known_hosts");
        let store = KnownHostsStore::open_at(&nested);
        store.trust("x.box", 22, &parse(KEY_A)).unwrap();
        assert!(nested.exists());
    }
}
