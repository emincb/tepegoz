//! Shared preflight checks for the xtask cross-compile toolchain.
//!
//! Consumed by both `cargo xtask demo-phase-6 up --remote` (single
//! target: `x86_64-unknown-linux-musl`) and `cargo xtask build-release`
//! (all four Decision #3 targets). One implementation, two callers —
//! the 6e-prep-3 round-2 regression happened because the original
//! preflight lived inside `demo_phase_6.rs` and was about to drift.
//!
//! ## Two-layer shape
//!
//! Cross-compiling to any Decision #3 target needs both:
//!
//! 1. **Rust std-lib for the target.** `rustup target list --installed`
//!    must include the target — rustc can't link without a precompiled
//!    `libstd`/`libcore` for the triple. OS-independent; macOS and Linux
//!    hosts both need it.
//! 2. **`cargo-zigbuild` on PATH.** Zig's embedded cross-toolchain is
//!    the universal linker: it can produce ELF-for-musl from macOS
//!    (ld64 can't), and Mach-O-for-darwin from macOS (ld64 can too, but
//!    zigbuild keeps the invocation shape uniform across all 4 targets).
//!
//! Conflating the two layers (accepting "zigbuild OR rustup-target") was
//! the round-2 regression shape: a macOS host with zigbuild but no musl
//! std trap-doored through a mid-build `can't find crate for 'core'`
//! leaving orphan fixture state behind. Preflight asserts BOTH layers
//! and emits a single composite install hint listing every missing
//! piece. Zero side effects before returning.
//!
//! ## R1 stance on Linux → darwin
//!
//! Cross-compiling to `*-apple-darwin` from a Linux host needs the
//! macOS SDK via `SDKROOT`, which we deliberately do not shim in R1 —
//! `docs/HANDOFF.md` flags it as "don't rabbit-hole." R2's GitHub
//! Actions workflow routes darwin targets to macos-latest so the
//! production path avoids Linux→darwin entirely. On a Linux host the
//! preflight rejects darwin targets up-front with "use a macOS host."
//!
//! ## zig version pin for ring cross-build
//!
//! `check_zig_ring_compatibility` is a separate check consumed only by
//! `build-release` (the controller binary links `ring` via russh, so
//! its cross-compile triggers cc-rs's asm build). Zig 0.16 ships with
//! llvm-ar 21, which regressed `ar cq libfoo.a foo.o` — the create-
//! if-missing semantics no longer work, failing ring's archive step
//! with "unable to open … libring_core_*.a: No such file or directory".
//! Zig 0.14 and 0.15 both work. The agent cross-build (via
//! `build-agents` + `demo-phase-6`) doesn't trigger this because
//! `tepegoz-agent` has no russh/ring dep.

use std::process::{Command, Stdio};

use anyhow::{Result, bail};

/// Decision #3 target triples Tepegöz cross-compiles both the agent
/// and the controller binary to. Kept in lockstep with the `TARGETS`
/// array in `crates/tepegoz/build.rs`.
pub(crate) const RELEASE_TARGETS: &[&str] = &[
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
];

/// Two-layer preflight for cross-compiling to each target via
/// `cargo zigbuild`. Bails with a composite install hint on any
/// missing layer; zero side effects on failure or success.
///
/// See module docs for the two-layer shape + R1 Linux→darwin stance.
pub(crate) fn check_cross_build_toolchain(targets: &[&str]) -> Result<()> {
    if cfg!(target_os = "linux") {
        let forbidden: Vec<&str> = targets
            .iter()
            .copied()
            .filter(|t| t.ends_with("-apple-darwin"))
            .collect();
        if !forbidden.is_empty() {
            bail!(
                "R1 preflight: cross-compiling to darwin from a Linux host is not supported \
                 (needs a macOS SDK shim we don't ship). Use a macOS host for darwin targets.\n\
                 Forbidden on this host: {}",
                forbidden.join(", ")
            );
        }
    }

    let missing_targets: Vec<&str> = targets
        .iter()
        .copied()
        .filter(|t| !rustup_has_target(t))
        .collect();
    let zigbuild_missing = !is_on_path("cargo-zigbuild");

    if missing_targets.is_empty() && !zigbuild_missing {
        return Ok(());
    }

    let mut summary = String::new();
    let mut install_lines: Vec<String> = Vec::new();

    if !missing_targets.is_empty() {
        summary.push_str("\n  - rustup targets (std-lib): ");
        summary.push_str(&missing_targets.join(", "));
        install_lines.push(format!("rustup target add {}", missing_targets.join(" ")));
    }
    if zigbuild_missing {
        summary.push_str(
            "\n  - cargo-zigbuild (linker; covers both musl and darwin via zig's cross-toolchain)",
        );
        install_lines.push("cargo install --locked cargo-zigbuild".into());
        if cfg!(target_os = "macos") {
            install_lines.push("brew install zig".into());
        } else {
            install_lines
                .push("# then install zig via your package manager (apt/dnf/pacman)".into());
        }
    }

    let install_block = install_lines.join("\n  && ");

    bail!(
        "cross-compile preflight failed. Missing on this host:{summary}\n\
         \n\
         Install:\n  {install_block}\n\
         \n\
         Then re-run."
    )
}

/// Check the zig version on PATH is compatible with ring's
/// cc-rs-driven asm cross-build. Zig 0.16+ ships with llvm-ar 21,
/// which regressed archive-creation semantics (`ar cq libfoo.a`
/// errors instead of creating), breaking ring 0.17.14's build step.
/// Zig 0.14 and 0.15 both work.
///
/// Returns `Ok(())` if zig is absent (that's the zigbuild check's
/// job to catch) or if zig version is 0.14.x / 0.15.x. Bails with an
/// install hint for incompatible versions.
///
/// Called only by `build-release` because the controller binary
/// pulls in `ring` via russh. `build-agents` / `demo-phase-6` don't
/// hit this since the agent binary has no russh dep.
pub(crate) fn check_zig_ring_compatibility() -> Result<()> {
    let output = Command::new("zig").arg("version").output();
    match output {
        Ok(out) if out.status.success() => {
            let raw = String::from_utf8_lossy(&out.stdout);
            let version = raw.trim();
            // Whitelist the two known-working minor versions. We
            // deliberately don't let ≥0.16 through with a warning —
            // the build WILL fail mid-link and the diagnostic ("ar:
            // unable to open libring_core_*.a") is obscure enough
            // that a cold reader can't trace it back to zig version.
            // 0.14 and 0.15 both side-step the llvm-ar 21 regression
            // that zig 0.16 ships. 0.14 additionally has a macOS 26
            // SDK-path bug (double-prefixes the SDK root for darwin
            // targets), so the hint recommends 0.15 explicitly.
            if version.starts_with("0.14.") || version.starts_with("0.15.") {
                Ok(())
            } else {
                bail!(
                    "zig {version} is incompatible with ring 0.17.14's cross-build (llvm-ar 21 \
                     regressed archive creation — `ar cq libfoo.a foo.o` no longer auto-creates \
                     the output). Install zig 0.15 (0.14 also works but has a macOS 26 SDK-path \
                     bug that can break darwin agent builds):\n\n  \
                     brew install zig@0.15\n  \
                     export PATH=\"/opt/homebrew/opt/zig@0.15/bin:$PATH\"\n\n\
                     Then re-run. (The agent cross-build isn't affected because `tepegoz-agent` \
                     has no russh/ring dep — this check only gates `build-release`.)"
                )
            }
        }
        // zig not on PATH → zigbuild check catches that separately
        // with its own install hint; don't double-error here.
        _ => Ok(()),
    }
}

/// Returns the first lipo-compatible tool on PATH: `lipo` (ships with
/// macOS Xcode command-line tools) or `llvm-lipo` (LLVM toolchain).
/// Returns `None` if neither is found.
///
/// R1 caller shape: `build_release` warns-and-skips the universal
/// macOS binary when this is `None`, but still produces the 4 per-
/// triple binaries. R2's CI workflow asserts `find_lipo().is_some()`
/// on macos-latest and fails red if absent — enforced at the
/// workflow layer, not here.
pub(crate) fn find_lipo() -> Option<&'static str> {
    if is_on_path("lipo") {
        Some("lipo")
    } else if is_on_path("llvm-lipo") {
        Some("llvm-lipo")
    } else {
        None
    }
}

/// Bails with `hint` if `binary` is not spawnable. Spawnability check,
/// not exit-code check — matches the reference model from
/// `xtask/src/demo.rs` (macOS OpenBSD `ssh-keygen --version` rejects
/// the flag with non-zero exit but is a legitimate install).
pub(crate) fn ensure_on_path(binary: &str, hint: &str) -> Result<()> {
    let result = Command::new(binary)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match result {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("`{binary}` not found on PATH. {hint}")
        }
        Err(e) => bail!("failed to probe `{binary}`: {e}. {hint}"),
    }
}

/// Non-bailing variant of `ensure_on_path`. Used by preflight paths
/// that branch on "is X installed?" before deciding the right
/// diagnostic to emit.
pub(crate) fn is_on_path(binary: &str) -> bool {
    Command::new(binary)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

/// Returns true iff `rustup target list --installed` lists `target`.
/// Returns false if rustup isn't on PATH or the subcommand fails —
/// conservative: treat "can't tell" as "not installed" so the user
/// gets the actionable install hint rather than a silent fall-through.
pub(crate) fn rustup_has_target(target: &str) -> bool {
    let output = Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output();
    match output {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .any(|l| l.trim() == target),
        _ => false,
    }
}
