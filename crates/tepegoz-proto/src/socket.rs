//! Default Unix socket path for Tepegöz.

use std::path::PathBuf;

/// Resolve the default per-user daemon socket path.
///
/// Order:
/// 1. `$XDG_RUNTIME_DIR/tepegoz-<uid>/daemon.sock` (Linux tmpfs, cleaned on logout)
/// 2. `$TMPDIR/tepegoz-<uid>/daemon.sock` (typical on macOS)
/// 3. `/tmp/tepegoz-<uid>/daemon.sock` (last-resort fallback)
///
/// The `<uid>` suffix prevents collisions on shared temp directories.
#[must_use]
pub fn default_socket_path() -> PathBuf {
    let base: PathBuf = std::env::var_os("XDG_RUNTIME_DIR")
        .or_else(|| std::env::var_os("TMPDIR"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));

    // SAFETY: `getuid` has no preconditions and always succeeds.
    let uid = unsafe { libc::getuid() };
    base.join(format!("tepegoz-{uid}")).join("daemon.sock")
}
