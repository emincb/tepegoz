//! Running-process probe — sysinfo-backed, stateful across samples.
//!
//! Unlike the ports probe (stateless: each `list_ports` call enumerates
//! sockets independently), the processes probe holds state across samples
//! because sysinfo computes CPU% as a delta between consecutive
//! `refresh_processes_specifics` calls. The first sample after
//! construction has no prior delta to compute against and returns
//! `cpu_percent: None` for every row — the TUI renders this as an em-dash
//! to disambiguate "not yet measured" from "idle / 0.0%".

use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};
use tepegoz_proto::ProbeProcess;

/// Human-readable identifier for the current implementation. Delivered in
/// `Event::ProcessList { source, .. }` so clients can surface it in the
/// tile footer (mirrors Docker's `engine_source`, Ports' `source`).
pub const SOURCE_LABEL: &str = "sysinfo";

/// Non-fatal errors from the processes probe.
///
/// The daemon's forward task treats any error here as a
/// `ProcessesUnavailable` event — it keeps retrying at its own cadence
/// and the TUI surfaces the reason.
#[derive(Debug, thiserror::Error)]
pub enum ProcessesError {
    /// sysinfo refresh panicked or returned a malformed snapshot. String
    /// is the underlying error verbatim.
    #[error("processes probe failed: {0}")]
    Backend(String),
}

/// Stateful processes probe.
///
/// Holds a sysinfo `System` across calls so CPU% delta computation works
/// correctly. Call [`ProcessesProbe::sample`] once per poll (e.g. every
/// 2 s) — the returned rows carry CPU% computed over the interval
/// between the previous call and this one.
pub struct ProcessesProbe {
    system: System,
    /// First `sample()` after construction returns `cpu_percent: None`
    /// for every row because there is no prior delta. Subsequent samples
    /// return `Some(x)`.
    first_sample: bool,
}

impl Default for ProcessesProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessesProbe {
    pub fn new() -> Self {
        Self {
            system: System::new_with_specifics(
                RefreshKind::new().with_processes(ProcessRefreshKind::new()),
            ),
            first_sample: true,
        }
    }

    /// Refresh the process list and return a per-process snapshot.
    ///
    /// Calling twice in quick succession yields valid CPU% deltas for the
    /// interval between the two calls; the first call after `new()`
    /// returns `cpu_percent: None` for every row.
    pub fn sample(&mut self) -> Result<Vec<ProbeProcess>, ProcessesError> {
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            ProcessRefreshKind::new().with_cpu().with_memory(),
        );

        let rows: Vec<ProbeProcess> = self
            .system
            .processes()
            .iter()
            .map(|(pid, proc)| {
                let argv: Vec<String> = proc
                    .cmd()
                    .iter()
                    .map(|os| os.to_string_lossy().to_string())
                    .collect();
                let command = if argv.is_empty() {
                    proc.name().to_string_lossy().to_string()
                } else {
                    argv.join(" ")
                };
                let partial = command.is_empty();

                ProbeProcess {
                    pid: pid.as_u32(),
                    parent_pid: proc.parent().map(|p| p.as_u32()).unwrap_or(0),
                    #[allow(clippy::cast_possible_wrap)]
                    start_time_unix_secs: proc.start_time() as i64,
                    command,
                    cpu_percent: if self.first_sample {
                        None
                    } else {
                        Some(proc.cpu_usage())
                    },
                    mem_bytes: proc.memory(),
                    partial,
                }
            })
            .collect();

        self.first_sample = false;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_sample_returns_cpu_none_for_every_row() {
        let mut probe = ProcessesProbe::new();
        let rows = probe.sample().expect("first sample should succeed");
        assert!(
            !rows.is_empty(),
            "at least one process must exist (the test runner itself)"
        );
        for row in &rows {
            assert!(
                row.cpu_percent.is_none(),
                "first sample must return cpu_percent: None so the TUI can \
                 render an em-dash instead of 0.0% (row={row:?})"
            );
        }
    }

    #[test]
    fn second_sample_returns_cpu_some_for_every_row() {
        let mut probe = ProcessesProbe::new();
        let _ = probe.sample().expect("first sample");
        // sysinfo needs a non-trivial interval to actually compute a delta,
        // but even a zero-delta second sample must carry Some(0.0) — the
        // type-level distinction is what matters here, not the value.
        std::thread::sleep(std::time::Duration::from_millis(50));
        let rows = probe.sample().expect("second sample");
        for row in &rows {
            assert!(
                row.cpu_percent.is_some(),
                "second sample onward must return cpu_percent: Some(_) \
                 (row={row:?})"
            );
        }
    }

    #[test]
    fn sample_contains_current_test_process() {
        let mut probe = ProcessesProbe::new();
        let rows = probe.sample().expect("sample");
        let test_pid = std::process::id();
        let found = rows
            .iter()
            .find(|r| r.pid == test_pid)
            .unwrap_or_else(|| panic!("own pid {test_pid} must appear in sample"));
        assert!(
            !found.command.is_empty(),
            "own command must be non-empty — we can read /proc/<ourpid>/cmdline \
             or the libproc equivalent without elevated privileges"
        );
        assert!(
            found.start_time_unix_secs > 0,
            "start_time must be a real Unix timestamp"
        );
        assert!(!found.partial);
    }
}
