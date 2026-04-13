//! Docker scope — bollard wrapper and socket discovery across Docker Desktop,
//! Colima, Rancher Desktop, and native Linux sockets.
//!
//! The daemon never panics because docker isn't around. [`Engine::connect`]
//! returns a structured error listing every candidate it tried; subscribers
//! see that as a [`tepegoz_proto::Event::DockerUnavailable`] and the daemon
//! retries on its own cadence.

use std::pin::Pin;
use std::time::Duration;

use bollard::Docker;
use bollard::container::LogOutput;
use bollard::query_parameters::{
    KillContainerOptionsBuilder, ListContainersOptionsBuilder, LogsOptionsBuilder,
    RemoveContainerOptionsBuilder, RestartContainerOptionsBuilder, StatsOptionsBuilder,
    StopContainerOptionsBuilder,
};
use futures_util::Stream;
use futures_util::StreamExt;
use tracing::{debug, info, warn};

use tepegoz_proto::{
    DockerActionKind, DockerContainer, DockerPort, DockerStats, LogStream as WireLogStream,
};

pub mod socket;

pub use socket::{SocketCandidate, discover_socket_candidates};

/// Connection timeout for any single bollard socket probe (seconds).
///
/// Default is hyper's connect timeout — too long for a daemon that wants to
/// fall through to the next candidate quickly. Five seconds is more than
/// enough for a local Unix socket.
const PROBE_TIMEOUT_SECS: u64 = 5;

/// Stream of container log chunks, already translated from `bollard::LogOutput`
/// to wire-side `(LogStream, Vec<u8>)`. Boxed so the daemon doesn't need to
/// know about the bollard type machinery.
pub type LogChunkStream =
    Pin<Box<dyn Stream<Item = anyhow::Result<(WireLogStream, Vec<u8>)>> + Send>>;

/// Stream of container resource samples, already translated from bollard's
/// `ContainerStatsResponse` to wire-side [`DockerStats`].
pub type StatsStream = Pin<Box<dyn Stream<Item = anyhow::Result<DockerStats>> + Send>>;

/// Engine wrapper around a connected `bollard::Docker`.
///
/// Cheap to clone — `bollard::Docker` is internally `Arc`-wrapped.
#[derive(Clone)]
pub struct Engine {
    docker: Docker,
    source: EngineSource,
}

/// Where the [`Engine`] connected: either an explicit `DOCKER_HOST` value or
/// one of the platform socket candidates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineSource {
    /// `DOCKER_HOST` was set; the value is preserved verbatim for diagnostics.
    DockerHostEnv(String),
    /// One of the platform socket candidates was reachable.
    Socket(SocketCandidate),
}

impl std::fmt::Display for EngineSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EngineSource::DockerHostEnv(v) => write!(f, "DOCKER_HOST={v}"),
            EngineSource::Socket(c) => write!(f, "{} ({})", c.label, c.path.display()),
        }
    }
}

impl Engine {
    /// Try to connect to a docker engine.
    ///
    /// Order: `DOCKER_HOST` env, then the platform socket candidate list in
    /// the order [`discover_socket_candidates`] returns. The first one that
    /// successfully pings wins.
    ///
    /// On failure, the returned error lists every attempt with its reason —
    /// the daemon surfaces this verbatim to subscribed clients so the user can
    /// see why docker is unavailable.
    pub async fn connect() -> Result<Self, ConnectError> {
        let mut attempts: Vec<(String, String)> = Vec::new();

        if let Ok(host) = std::env::var("DOCKER_HOST") {
            if !host.is_empty() {
                let label = format!("DOCKER_HOST={host}");
                match try_connect_docker_host(&host).await {
                    Ok(docker) => {
                        info!(source = %label, "docker engine connected");
                        return Ok(Self {
                            docker,
                            source: EngineSource::DockerHostEnv(host),
                        });
                    }
                    Err(e) => {
                        debug!(source = %label, error = %e, "docker connect failed");
                        attempts.push((label, e.to_string()));
                    }
                }
            }
        }

        for candidate in discover_socket_candidates() {
            if !candidate.path.exists() {
                attempts.push((candidate.label.to_string(), "socket file not found".into()));
                continue;
            }
            let label = format!("{} ({})", candidate.label, candidate.path.display());
            match try_connect_socket(&candidate.path).await {
                Ok(docker) => {
                    info!(source = %label, "docker engine connected");
                    return Ok(Self {
                        docker,
                        source: EngineSource::Socket(candidate),
                    });
                }
                Err(e) => {
                    debug!(source = %label, error = %e, "docker connect failed");
                    attempts.push((label, e.to_string()));
                }
            }
        }

        Err(ConnectError { attempts })
    }

    /// Source of the engine connection (which `DOCKER_HOST` value or socket).
    #[must_use]
    pub fn source(&self) -> &EngineSource {
        &self.source
    }

    /// Underlying bollard handle for code that needs the raw API (e.g. logs
    /// streaming, exec, lifecycle actions in later slices).
    #[must_use]
    pub fn raw(&self) -> &Docker {
        &self.docker
    }

    /// List all containers (running + stopped). Translated to wire types.
    pub async fn list_containers(&self) -> anyhow::Result<Vec<DockerContainer>> {
        let opts = ListContainersOptionsBuilder::new().all(true).build();
        let summaries = self.docker.list_containers(Some(opts)).await?;
        Ok(summaries.into_iter().map(into_wire).collect())
    }

    /// Run a one-shot lifecycle action against a container. Maps the wire
    /// `DockerActionKind` to the matching bollard call.
    ///
    /// `Remove` uses force-remove so the action works on running containers
    /// too — that matches `docker rm -f` and what the TUI's "remove" key is
    /// expected to do.
    pub async fn action(&self, container_id: &str, kind: DockerActionKind) -> anyhow::Result<()> {
        match kind {
            DockerActionKind::Start => {
                self.docker.start_container(container_id, None).await?;
            }
            DockerActionKind::Stop => {
                let opts = StopContainerOptionsBuilder::new().build();
                self.docker.stop_container(container_id, Some(opts)).await?;
            }
            DockerActionKind::Restart => {
                let opts = RestartContainerOptionsBuilder::new().build();
                self.docker
                    .restart_container(container_id, Some(opts))
                    .await?;
            }
            DockerActionKind::Kill => {
                let opts = KillContainerOptionsBuilder::new().build();
                self.docker.kill_container(container_id, Some(opts)).await?;
            }
            DockerActionKind::Remove => {
                let opts = RemoveContainerOptionsBuilder::new().force(true).build();
                self.docker
                    .remove_container(container_id, Some(opts))
                    .await?;
            }
        }
        Ok(())
    }

    /// Stream a container's logs.
    ///
    /// `tail_lines = 0` means "all" (matches bollard's `tail = "all"`
    /// default — keeps the wire knob intuitive). `follow = true` keeps the
    /// stream open until the container exits or the engine goes away.
    /// Each yielded item is a `(LogStream, bytes)` pair already translated
    /// to wire types so the daemon doesn't need to know about bollard.
    pub fn logs_stream(&self, container_id: &str, follow: bool, tail_lines: u32) -> LogChunkStream {
        let tail = if tail_lines == 0 {
            "all".to_string()
        } else {
            tail_lines.to_string()
        };
        let opts = LogsOptionsBuilder::new()
            .stdout(true)
            .stderr(true)
            .follow(follow)
            .tail(&tail)
            .build();
        let stream = self.docker.logs(container_id, Some(opts));
        Box::pin(stream.filter_map(|item| async move {
            match item {
                Ok(LogOutput::StdOut { message }) => {
                    Some(Ok((WireLogStream::Stdout, message.to_vec())))
                }
                Ok(LogOutput::StdErr { message }) => {
                    Some(Ok((WireLogStream::Stderr, message.to_vec())))
                }
                // Console (raw tty) and StdIn aren't meaningful for our log
                // panel — treat both as stdout so the user still sees them.
                Ok(LogOutput::Console { message }) => {
                    Some(Ok((WireLogStream::Stdout, message.to_vec())))
                }
                Ok(LogOutput::StdIn { .. }) => None,
                Err(e) => Some(Err(anyhow::anyhow!("docker logs: {e}"))),
            }
        }))
    }

    /// Stream a container's stats samples (~1/sec). Each item is already
    /// translated to wire types (`DockerStats`); CPU% comes from the
    /// standard docker stats CLI formula and is `0.0` whenever a delta
    /// can't be calculated (first sample, missing precpu on Windows).
    pub fn stats_stream(&self, container_id: &str) -> StatsStream {
        let opts = StatsOptionsBuilder::new().stream(true).build();
        let stream = self.docker.stats(container_id, Some(opts));
        Box::pin(stream.map(|item| {
            item.map(stats_to_wire)
                .map_err(|e| anyhow::anyhow!("docker stats: {e}"))
        }))
    }
}

/// Structured failure: every connection attempt that was made and why it
/// failed. Daemon surfaces this as the `reason` in `DockerUnavailable`.
#[derive(Debug)]
pub struct ConnectError {
    pub attempts: Vec<(String, String)>,
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.attempts.is_empty() {
            return f.write_str("no docker socket candidates available");
        }
        f.write_str("docker engine unreachable. Tried:")?;
        for (src, err) in &self.attempts {
            write!(f, "\n  - {src}: {err}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ConnectError {}

async fn try_connect_docker_host(host: &str) -> anyhow::Result<Docker> {
    // bollard::Docker::connect_with_host handles unix://, tcp://, http://,
    // ssh:// and named pipe schemes — we don't second-guess the user's URL.
    let docker = Docker::connect_with_host(host)?;
    ping_with_timeout(&docker).await?;
    Ok(docker)
}

async fn try_connect_socket(path: &std::path::Path) -> anyhow::Result<Docker> {
    let path_str = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("socket path is not valid UTF-8: {}", path.display()))?;
    // PROBE_TIMEOUT_SECS is in seconds for hyper's read/write timeout. The
    // ping below adds its own outer timeout in case the daemon stalls on
    // accept rather than I/O.
    let docker =
        Docker::connect_with_socket(path_str, PROBE_TIMEOUT_SECS, bollard::API_DEFAULT_VERSION)?;
    ping_with_timeout(&docker).await?;
    Ok(docker)
}

async fn ping_with_timeout(docker: &Docker) -> anyhow::Result<()> {
    match tokio::time::timeout(Duration::from_secs(PROBE_TIMEOUT_SECS), docker.ping()).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(anyhow::anyhow!("ping: {e}")),
        Err(_) => {
            warn!("docker ping exceeded {PROBE_TIMEOUT_SECS}s");
            Err(anyhow::anyhow!(
                "ping timed out after {PROBE_TIMEOUT_SECS}s"
            ))
        }
    }
}

fn into_wire(s: bollard::models::ContainerSummary) -> DockerContainer {
    use bollard::models::ContainerSummaryStateEnum;

    let state = match s.state {
        Some(ContainerSummaryStateEnum::EMPTY) | None => "unknown".to_string(),
        Some(other) => other.to_string(),
    };

    let ports = s
        .ports
        .unwrap_or_default()
        .into_iter()
        .map(|p| DockerPort {
            ip: p.ip.unwrap_or_default(),
            private_port: p.private_port,
            public_port: p.public_port.unwrap_or(0),
            protocol: p
                .typ
                .map(|t| t.to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "tcp".into()),
        })
        .collect();

    let mut labels: Vec<(String, String)> = s.labels.unwrap_or_default().into_iter().collect();
    labels.sort_by(|a, b| a.0.cmp(&b.0));

    DockerContainer {
        id: s.id.unwrap_or_default(),
        names: s.names.unwrap_or_default(),
        image: s.image.unwrap_or_default(),
        image_id: s.image_id.unwrap_or_default(),
        command: s.command.unwrap_or_default(),
        created_unix_secs: s.created.unwrap_or(0),
        state,
        status: s.status.unwrap_or_default(),
        ports,
        labels: labels
            .into_iter()
            .map(|(key, value)| tepegoz_proto::KeyValue { key, value })
            .collect(),
    }
}

/// Translate a bollard `ContainerStatsResponse` into our wire-side
/// [`DockerStats`].
///
/// CPU calculation matches the standard formula used by the `docker stats`
/// CLI: `(cpu_delta / system_delta) * online_cpus * 100`, where the deltas
/// are computed against the matching `precpu_stats` fields. `0.0` whenever
/// any field needed for the calculation is missing or `system_delta == 0`,
/// which is normal on the first sample of a stream.
fn stats_to_wire(s: bollard::models::ContainerStatsResponse) -> DockerStats {
    let cpu_percent = s
        .cpu_stats
        .as_ref()
        .and_then(|cs| {
            let pre = s.precpu_stats.as_ref()?;
            let cpu_total = cs.cpu_usage.as_ref()?.total_usage?;
            let pre_total = pre.cpu_usage.as_ref()?.total_usage?;
            let sys_now = cs.system_cpu_usage?;
            let sys_pre = pre.system_cpu_usage?;
            let online = cs.online_cpus.unwrap_or(1).max(1) as f32;
            if sys_now <= sys_pre || cpu_total <= pre_total {
                return Some(0.0_f32);
            }
            let cpu_delta = (cpu_total - pre_total) as f32;
            let sys_delta = (sys_now - sys_pre) as f32;
            Some((cpu_delta / sys_delta) * online * 100.0)
        })
        .unwrap_or(0.0);

    let (mem_bytes, mem_limit_bytes) = s
        .memory_stats
        .as_ref()
        .map(|m| (m.usage.unwrap_or(0), m.limit.unwrap_or(0)))
        .unwrap_or((0, 0));

    DockerStats {
        cpu_percent,
        mem_bytes,
        mem_limit_bytes,
    }
}

/// Path on disk used by docker socket discovery — exposed for testing.
#[doc(hidden)]
pub use socket::candidate_paths_for_test;

#[cfg(test)]
mod tests {
    use super::*;
    use bollard::models::{
        ContainerSummary, ContainerSummaryStateEnum, PortSummary, PortSummaryTypeEnum,
    };
    use std::collections::HashMap;

    /// `into_wire` is what the Available path serves to clients. Since we
    /// can't run a real docker daemon in unit tests, exercise the translation
    /// directly: every bollard field maps to its wire counterpart, optional
    /// fields collapse to safe defaults, and labels come out sorted.
    #[test]
    fn into_wire_translates_bollard_summary() {
        let mut labels = HashMap::new();
        labels.insert("zlabel".into(), "zvalue".into());
        labels.insert("alabel".into(), "avalue".into());

        let summary = ContainerSummary {
            id: Some("abc123".into()),
            names: Some(vec!["/myapp".into()]),
            image: Some("nginx:latest".into()),
            image_id: Some("sha256:deadbeef".into()),
            command: Some("nginx -g daemon off;".into()),
            created: Some(1_700_000_000),
            state: Some(ContainerSummaryStateEnum::RUNNING),
            status: Some("Up 5 minutes".into()),
            ports: Some(vec![
                PortSummary {
                    ip: Some("0.0.0.0".into()),
                    private_port: 80,
                    public_port: Some(8080),
                    typ: Some(PortSummaryTypeEnum::TCP),
                },
                PortSummary {
                    ip: None,
                    private_port: 443,
                    public_port: None,
                    typ: None,
                },
            ]),
            labels: Some(labels),
            ..Default::default()
        };

        let wire = into_wire(summary);

        assert_eq!(wire.id, "abc123");
        assert_eq!(wire.names, vec!["/myapp".to_string()]);
        assert_eq!(wire.image, "nginx:latest");
        assert_eq!(wire.image_id, "sha256:deadbeef");
        assert_eq!(wire.created_unix_secs, 1_700_000_000);
        assert_eq!(wire.state, "running");
        assert_eq!(wire.status, "Up 5 minutes");
        assert_eq!(wire.ports.len(), 2);
        assert_eq!(wire.ports[0].ip, "0.0.0.0");
        assert_eq!(wire.ports[0].public_port, 8080);
        assert_eq!(wire.ports[0].protocol, "tcp");
        // Missing port type → "tcp" default; missing public_port → 0.
        assert_eq!(wire.ports[1].ip, "");
        assert_eq!(wire.ports[1].public_port, 0);
        assert_eq!(wire.ports[1].protocol, "tcp");
        // Labels sorted by key — UI is allowed to depend on stable order.
        assert_eq!(wire.labels.len(), 2);
        assert_eq!(wire.labels[0].key, "alabel");
        assert_eq!(wire.labels[1].key, "zlabel");
    }

    /// Empty / unset state must come out as "unknown" rather than crashing
    /// the wire encoder or showing the user a blank cell.
    #[test]
    fn into_wire_handles_empty_state() {
        let summary = ContainerSummary {
            state: Some(ContainerSummaryStateEnum::EMPTY),
            ..Default::default()
        };
        assert_eq!(into_wire(summary).state, "unknown");

        let summary = ContainerSummary {
            state: None,
            ..Default::default()
        };
        assert_eq!(into_wire(summary).state, "unknown");
    }

    /// CPU% formula: `(cpu_delta / sys_delta) * online_cpus * 100`.
    /// With cpu_total = 200, pre_total = 100, sys_now = 1000, sys_pre = 500,
    /// online = 4 → (100 / 500) * 4 * 100 = 80.0%.
    #[test]
    fn stats_to_wire_computes_cpu_percent() {
        use bollard::models::{
            ContainerCpuStats, ContainerCpuUsage, ContainerMemoryStats, ContainerStatsResponse,
        };

        let response = ContainerStatsResponse {
            cpu_stats: Some(ContainerCpuStats {
                cpu_usage: Some(ContainerCpuUsage {
                    total_usage: Some(200),
                    ..Default::default()
                }),
                system_cpu_usage: Some(1000),
                online_cpus: Some(4),
                ..Default::default()
            }),
            precpu_stats: Some(ContainerCpuStats {
                cpu_usage: Some(ContainerCpuUsage {
                    total_usage: Some(100),
                    ..Default::default()
                }),
                system_cpu_usage: Some(500),
                ..Default::default()
            }),
            memory_stats: Some(ContainerMemoryStats {
                usage: Some(2_147_483_648),
                limit: Some(8_589_934_592),
                ..Default::default()
            }),
            ..Default::default()
        };

        let stats = stats_to_wire(response);
        assert!(
            (stats.cpu_percent - 80.0).abs() < 0.001,
            "expected 80.0%, got {}",
            stats.cpu_percent
        );
        assert_eq!(stats.mem_bytes, 2_147_483_648);
        assert_eq!(stats.mem_limit_bytes, 8_589_934_592);
    }

    /// First sample / missing precpu / sys_delta=0 → cpu_percent must be
    /// 0.0, not NaN, not a divide-by-zero, not a panic.
    #[test]
    fn stats_to_wire_returns_zero_cpu_when_delta_is_unavailable() {
        use bollard::models::{ContainerCpuStats, ContainerCpuUsage, ContainerStatsResponse};

        // No precpu_stats at all.
        let response = ContainerStatsResponse {
            cpu_stats: Some(ContainerCpuStats {
                cpu_usage: Some(ContainerCpuUsage {
                    total_usage: Some(200),
                    ..Default::default()
                }),
                system_cpu_usage: Some(1000),
                online_cpus: Some(4),
                ..Default::default()
            }),
            precpu_stats: None,
            ..Default::default()
        };
        assert_eq!(stats_to_wire(response).cpu_percent, 0.0);

        // sys_delta = 0 (sys_now == sys_pre).
        let response = ContainerStatsResponse {
            cpu_stats: Some(ContainerCpuStats {
                cpu_usage: Some(ContainerCpuUsage {
                    total_usage: Some(200),
                    ..Default::default()
                }),
                system_cpu_usage: Some(1000),
                online_cpus: Some(4),
                ..Default::default()
            }),
            precpu_stats: Some(ContainerCpuStats {
                cpu_usage: Some(ContainerCpuUsage {
                    total_usage: Some(100),
                    ..Default::default()
                }),
                system_cpu_usage: Some(1000),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(stats_to_wire(response).cpu_percent, 0.0);
    }

    /// Memory section missing entirely → both fields default to 0 (no panic,
    /// no overflow, just a sensibly-empty stats sample).
    #[test]
    fn stats_to_wire_handles_missing_memory_section() {
        use bollard::models::ContainerStatsResponse;
        let response = ContainerStatsResponse {
            memory_stats: None,
            ..Default::default()
        };
        let stats = stats_to_wire(response);
        assert_eq!(stats.mem_bytes, 0);
        assert_eq!(stats.mem_limit_bytes, 0);
        assert_eq!(stats.cpu_percent, 0.0);
    }
}
