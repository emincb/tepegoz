//! macOS-native helpers.
//!
//! Phase 4 Slice 4a does not ship any macOS-specific helpers here. Listening
//! socket enumeration + pid attribution go through the cross-OS `netstat2`
//! wrapper (which delegates to libproc under the hood), and pid → process
//! name is resolved by `sysinfo`. Pid → container correlation cannot happen
//! at the probe layer on macOS because Docker Desktop runs containers
//! inside a Linux VM — the macOS-visible pid is the VM host, not the
//! in-container process. Correlation therefore completes in the daemon
//! (`tepegoz-core`) by cross-referencing port numbers against bollard's
//! container list.
//!
//! Reserved for future sysctl / libproc helpers that don't fit the cross-OS
//! wrapper shape (e.g., higher-resolution cpu / mem sampling).
