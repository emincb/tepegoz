//! Emits `pub const PROTOCOL_VERSION: u32 = …` to `OUT_DIR/version.rs`
//! from the plain-text source of truth at `PROTOCOL_VERSION`.
//!
//! Why not declare the const directly in `src/lib.rs`? Because every
//! other build.rs in the workspace that needs the protocol version
//! — `tepegoz-agent` (manifest-write), `tepegoz` controller (embed +
//! version-drift check), `xtask::build_agents` (at runtime) — would
//! otherwise have to reach into the proto crate's AST or duplicate
//! the literal. Centralising the version as a plain-text file keeps
//! every consumer pointing at the same source of truth with a
//! trivial `fs::read_to_string` — no AST parsing, no macro crate,
//! no duplicate literal to drift out of sync.
//!
//! The `cargo:rerun-if-changed=PROTOCOL_VERSION` line means a bump
//! to the file forces a recompile of every dependent crate.

use std::env;
use std::fs;
use std::path::PathBuf;

const VERSION_FILE: &str = "PROTOCOL_VERSION";

fn main() {
    println!("cargo:rerun-if-changed={VERSION_FILE}");
    println!("cargo:rerun-if-changed=build.rs");

    let raw = fs::read_to_string(VERSION_FILE)
        .unwrap_or_else(|e| panic!("failed to read {VERSION_FILE}: {e}"));
    let trimmed = raw.trim();
    let version: u32 = trimmed.parse().unwrap_or_else(|e| {
        panic!("{VERSION_FILE} must contain a u32 decimal literal, got {trimmed:?}: {e}")
    });

    let out_dir = env::var_os("OUT_DIR").expect("OUT_DIR set by cargo");
    let out_path = PathBuf::from(out_dir).join("version.rs");
    let body = format!(
        "/// Wire protocol version. Generated at build time from\n\
         /// `crates/tepegoz-proto/PROTOCOL_VERSION` — see build.rs.\n\
         pub const PROTOCOL_VERSION: u32 = {version};\n"
    );
    fs::write(&out_path, body)
        .unwrap_or_else(|e| panic!("failed to write {}: {e}", out_path.display()));
}
