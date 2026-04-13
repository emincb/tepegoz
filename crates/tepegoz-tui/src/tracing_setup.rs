//! File-backed tracing for the TUI.
//!
//! The TUI owns stdout for pty passthrough — writing tracing events there
//! would corrupt the display. Logs go to a file instead, configurable via
//! `TEPEGOZ_LOG_FILE`.

use std::path::PathBuf;

use tracing_subscriber::EnvFilter;

pub(crate) fn init(default_level: &str) -> anyhow::Result<()> {
    let log_path = resolve_log_path();
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Fail loudly at startup if the log path is unwritable.
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;

    let writer_path = log_path.clone();
    let default_directive = default_level
        .parse()
        .unwrap_or_else(|_| tracing::Level::INFO.into());
    let filter = EnvFilter::builder()
        .with_default_directive(default_directive)
        .with_env_var("RUST_LOG")
        .from_env_lossy();

    tracing_subscriber::fmt()
        .with_writer(move || {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&writer_path)
                .unwrap_or_else(|_| std::fs::File::create("/dev/null").expect("null sink"))
        })
        .with_ansi(false)
        .with_env_filter(filter)
        .init();

    tracing::info!(log_path = %log_path.display(), "tepegoz tui starting");
    Ok(())
}

fn resolve_log_path() -> PathBuf {
    if let Some(p) = std::env::var_os("TEPEGOZ_LOG_FILE") {
        return PathBuf::from(p);
    }
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("tepegoz").join("tui.log")
}
