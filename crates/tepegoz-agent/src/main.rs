//! Tepegöz remote agent entry point.
//!
//! Phase 6 Slice 6a shape: a stdio-framed protocol server. No CLI
//! flags today — the binary blocks until stdin closes. Tracing is
//! routed to stderr so the wire protocol on stdout stays clean; set
//! `RUST_LOG=debug` to enable verbose diagnostics during a deploy.

use anyhow::Context;
use tracing_subscriber::EnvFilter;

fn main() -> anyhow::Result<()> {
    // Tracing to stderr only — stdout is the wire. Default level is
    // warn to keep remote logs terse; user opts into debug via
    // RUST_LOG=debug.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    runtime.block_on(tepegoz_agent::run_stdio())
}
