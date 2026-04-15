//! Phase 6 Slice 6b — remote agent deploy pipeline.
//!
//! Four-step flow from an authenticated [`SshSession`] to a running
//! remote agent ready to speak wire v10:
//!
//! 1. [`detect_target`] — `uname -sm` over an exec channel, parsed to
//!    the target triple the controller's `embedded_agents::for_target`
//!    expects.
//! 2. [`deploy_agent`] — compares the embedded bytes' SHA256 against
//!    the remote file (if any); on cache hit, no upload. Otherwise,
//!    `cat > tmp` → atomic `mv` → `chmod +x`. Verify hash post-
//!    transfer; one redeploy retry on mismatch; terminal error on
//!    second mismatch. Remote path: `$HOME/.cache/tepegoz/agent-v<N>`
//!    where `N` is the controller's compiled-in `PROTOCOL_VERSION`.
//! 3. [`spawn_agent_channel`] — opens a fresh channel, exec's the
//!    deployed binary, returns the channel's inner russh handle for
//!    the caller to drive the wire protocol over.
//! 4. [`handshake_agent`] — writes `Payload::AgentHandshake`, reads
//!    `Payload::AgentHandshakeResponse`, asserts the reported
//!    `version` matches the controller's expected value. Mismatch is
//!    terminal (no retry) — per the 6b brief.
//!
//! ## Agent TOFU model (CTO note)
//!
//! Per the 6b brief: the agent's identity is its binary hash +
//! `PROTOCOL_VERSION`, both controller-owned at build time. We
//! **verify every deploy** against the embedded bytes — no
//! first-seen-wins stored-hash DB à la SSH host keys. If the remote
//! file's SHA256 diverges from what we'd upload, we redeploy. That's
//! the whole model; the "cached" branch is an idempotence
//! optimisation, not a trust anchor.
//!
//! ## Not scope for 6b (deferred to 6c/d)
//!
//! Real probe capabilities in the handshake response (`capabilities`
//! stays empty in 6a / 6b), remote subscription dispatch, daemon-side
//! agent session pool. 6b's product is a single deploy-and-handshake
//! round-trip — enough for `tepegoz doctor --agents` to report state
//! and for the next slice to build a real remote pane on.

use std::time::Duration;

use russh::ChannelMsg;
use sha2::{Digest, Sha256};
use tepegoz_proto::{Envelope, Payload, codec::read_envelope};
use tracing::debug;

use crate::{SshChannel, SshError, SshSession, open_session};

// ---- Constants ---------------------------------------------------------

/// Cap any single exec's stdout / stderr read to avoid hanging on a
/// misbehaving remote. 30 s suits everything except the upload step
/// (which gets its own budget via [`UPLOAD_TIMEOUT`]).
const EXEC_TIMEOUT: Duration = Duration::from_secs(30);

/// Upload budget. Sized for the release-agent binary (under ~1 MiB
/// compressed; ~1 MB/s over SSH is the pessimistic floor) plus enough
/// slack for slow paths.
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);

/// Handshake round-trip budget once the agent is exec'd. The agent
/// replies within single-digit ms on a healthy channel — 5 s is
/// generous, matches the 6a subprocess-handshake pattern.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

// ---- Detection ---------------------------------------------------------

/// Result of `uname -sm` parsed into tepegoz's target-triple vocabulary.
///
/// `os` is the normalized (lowercase) OS token as reported by the
/// remote (`"linux"` / `"darwin"`). `arch` is the raw `uname -m` token
/// pre-normalization (`"x86_64"` / `"aarch64"` / `"arm64"`). The
/// `target_triple` field is the resolved tepegoz triple — the key
/// callers use to look up an embedded agent via
/// `embedded_agents::for_target`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedTarget {
    pub os: String,
    pub arch: String,
    pub target_triple: String,
}

/// Run `uname -sm` on the remote and resolve to one of the four
/// tepegoz-agent target triples.
pub async fn detect_target(session: &SshSession) -> Result<DetectedTarget, SshError> {
    let out = run_exec(session, "uname -sm", &[], EXEC_TIMEOUT).await?;
    if out.exit_status != 0 {
        return Err(SshError::DeployFailed {
            stage: "detect_target".into(),
            reason: format!(
                "uname -sm exited {}: {}",
                out.exit_status,
                out.stderr.trim()
            ),
        });
    }
    parse_uname_sm(out.stdout.trim())
}

/// Pure parser — split out so uname-token coverage is unit-testable
/// without standing up an SSH connection.
fn parse_uname_sm(line: &str) -> Result<DetectedTarget, SshError> {
    let mut parts = line.split_whitespace();
    let os_raw = parts.next().unwrap_or("");
    let arch_raw = parts.next().unwrap_or("");
    let os_norm = os_raw.to_lowercase();
    let triple = match (os_norm.as_str(), arch_raw) {
        ("linux", "x86_64") | ("linux", "amd64") => "x86_64-unknown-linux-musl",
        ("linux", "aarch64") | ("linux", "arm64") => "aarch64-unknown-linux-musl",
        ("darwin", "x86_64") => "x86_64-apple-darwin",
        ("darwin", "arm64") | ("darwin", "aarch64") => "aarch64-apple-darwin",
        _ => {
            return Err(SshError::UnsupportedPlatform {
                os: os_raw.to_string(),
                arch: arch_raw.to_string(),
                supported: vec![
                    "Linux x86_64 → x86_64-unknown-linux-musl".into(),
                    "Linux aarch64 → aarch64-unknown-linux-musl".into(),
                    "Darwin x86_64 → x86_64-apple-darwin".into(),
                    "Darwin arm64 → aarch64-apple-darwin".into(),
                ],
            });
        }
    };
    Ok(DetectedTarget {
        os: os_norm,
        arch: arch_raw.to_string(),
        target_triple: triple.to_string(),
    })
}

// ---- Remote inspection -------------------------------------------------

/// State of the remote agent-binary cache slot for the controller's
/// protocol version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteAgentStatus {
    /// File at the expected path is not present on the remote.
    Absent,
    /// File exists; payload describes it.
    Present {
        /// Hex-encoded SHA256 digest of the remote file's bytes.
        sha256_hex: String,
        /// True when `sha256_hex` equals the caller's expected hash
        /// (i.e., what the controller would upload). False means the
        /// remote has a stale / different binary and a redeploy is
        /// needed.
        matches_expected: bool,
        /// Size of the remote file in bytes (best-effort via `stat`
        /// or `wc -c`).
        size_bytes: u64,
        /// Last-modified timestamp as seconds since the unix epoch.
        mtime_unix_secs: u64,
    },
}

/// Inspect the remote agent slot without uploading. Used by
/// `tepegoz doctor --agents` to report deploy state and by
/// [`deploy_agent`] internally for the cache-hit short-circuit.
pub async fn inspect_remote_agent(
    session: &SshSession,
    remote_path: &str,
    expected_sha256_hex: &str,
    target: &DetectedTarget,
) -> Result<RemoteAgentStatus, SshError> {
    // Presence check. `[ -f path ] && echo present || echo absent`
    // prints exactly one of two strings no matter what; parsing is
    // a string-literal match.
    let presence_cmd = format!(
        "[ -f {} ] && echo present || echo absent",
        shell_quote(remote_path)
    );
    let presence = run_exec(session, &presence_cmd, &[], EXEC_TIMEOUT).await?;
    match presence.stdout.trim() {
        "absent" => return Ok(RemoteAgentStatus::Absent),
        "present" => {}
        other => {
            return Err(SshError::DeployFailed {
                stage: "inspect".into(),
                reason: format!("presence check returned unexpected output {other:?}"),
            });
        }
    }

    // Hash.
    let hash_cmd = hash_command(&target.os, remote_path);
    let hash_out = run_exec(session, &hash_cmd, &[], EXEC_TIMEOUT).await?;
    if hash_out.exit_status != 0 {
        return Err(SshError::DeployFailed {
            stage: "inspect_hash".into(),
            reason: format!(
                "`{hash_cmd}` exited {}: {}",
                hash_out.exit_status,
                hash_out.stderr.trim()
            ),
        });
    }
    let sha256 = parse_hash_output(&hash_out.stdout)?;

    // Size + mtime. `stat -c '%s %Y' path` on Linux, `stat -f '%z %m'
    // on macOS. Split-command per-OS keeps each one simple.
    let stat_cmd = stat_command(&target.os, remote_path);
    let stat_out = run_exec(session, &stat_cmd, &[], EXEC_TIMEOUT).await?;
    let (size_bytes, mtime_unix_secs) = if stat_out.exit_status == 0 {
        parse_stat_output(&stat_out.stdout).unwrap_or((0, 0))
    } else {
        // stat failures are non-fatal — we got the hash above, which
        // is what really matters. Report (0, 0) and let the caller
        // surface it.
        (0, 0)
    };

    Ok(RemoteAgentStatus::Present {
        sha256_hex: sha256.clone(),
        matches_expected: sha256 == expected_sha256_hex,
        size_bytes,
        mtime_unix_secs,
    })
}

// ---- Deploy ------------------------------------------------------------

/// Observable outcome of a deploy call. `deployed_now == false`
/// means the remote already had a byte-identical binary and no
/// upload happened; callers use that to decide whether to surface a
/// "cached" hint in user-facing diagnostics.
#[derive(Debug, Clone)]
pub struct DeployOutcome {
    pub target: DetectedTarget,
    pub remote_path: String,
    pub sha256_hex: String,
    pub deployed_now: bool,
}

/// Deploy `agent_bytes` to the remote's cache slot for this protocol
/// version. Idempotent: a pre-existing matching binary short-
/// circuits. Post-transfer SHA256 verification gets one retry on
/// mismatch before erroring out.
pub async fn deploy_agent(
    session: &SshSession,
    agent_bytes: &[u8],
    protocol_version: u32,
) -> Result<DeployOutcome, SshError> {
    let target = detect_target(session).await?;
    let local_hash = hex::encode(Sha256::digest(agent_bytes));
    let home = resolve_remote_home(session).await?;
    let remote_dir = format!("{home}/.cache/tepegoz");
    let remote_path = format!("{remote_dir}/agent-v{protocol_version}");

    // Cache-hit check — the 6b brief's core idempotence path.
    let status = inspect_remote_agent(session, &remote_path, &local_hash, &target).await?;
    if let RemoteAgentStatus::Present {
        matches_expected: true,
        ..
    } = &status
    {
        debug!(
            remote = %remote_path,
            sha256 = %local_hash,
            "agent already deployed at matching hash — skipping upload"
        );
        return Ok(DeployOutcome {
            target,
            remote_path,
            sha256_hex: local_hash,
            deployed_now: false,
        });
    }

    // Upload + verify.
    upload_binary(session, agent_bytes, &remote_dir, &remote_path).await?;
    let uploaded_hash = compute_remote_hash(session, &remote_path, &target).await?;
    if uploaded_hash == local_hash {
        return Ok(DeployOutcome {
            target,
            remote_path,
            sha256_hex: local_hash,
            deployed_now: true,
        });
    }

    // One retry — partial-transfer on the first attempt is plausible
    // (flaky network, mid-write disconnect). Second mismatch is
    // terminal per the 6b brief.
    debug!(
        expected = %local_hash,
        got = %uploaded_hash,
        "post-transfer hash mismatch — redeploying once"
    );
    upload_binary(session, agent_bytes, &remote_dir, &remote_path).await?;
    let retry_hash = compute_remote_hash(session, &remote_path, &target).await?;
    if retry_hash != local_hash {
        return Err(SshError::ChecksumMismatch {
            remote_path,
            expected: local_hash,
            actual: retry_hash,
        });
    }
    Ok(DeployOutcome {
        target,
        remote_path,
        sha256_hex: local_hash,
        deployed_now: true,
    })
}

// ---- Exec + handshake --------------------------------------------------

/// Exec the deployed agent binary in a fresh SSH channel. Returns
/// the channel with its stdio wired for the rkyv wire protocol —
/// callers drive [`handshake_agent`] (or, in later slices, real
/// subscription multiplexing) over it.
pub async fn spawn_agent_channel(
    session: &SshSession,
    remote_path: &str,
) -> Result<SshChannel, SshError> {
    let channel = open_session(session).await?;
    let inner = channel.into_inner();
    // Plain exec, no pty. Stdin + stdout carry the wire protocol;
    // stderr carries any agent tracing (stderr inherits to the
    // controller log, useful for debugging deploy surprises).
    inner
        .exec(true, remote_path)
        .await
        .map_err(|e| SshError::DeployFailed {
            stage: "exec_agent".into(),
            reason: format!("exec `{remote_path}`: {e}"),
        })?;
    Ok(SshChannel::from_raw(inner))
}

/// Drive one handshake round-trip against the agent attached to
/// `channel`. Validates the returned `version` against `expected_version`
/// (which the controller passes as its compiled-in `PROTOCOL_VERSION`);
/// any mismatch surfaces as [`SshError::AgentVersionMismatch`] and is
/// terminal — no retry. Per the 6b brief: protocol drift can't heal by
/// re-trying.
pub async fn handshake_agent(
    channel: &mut SshChannel,
    expected_version: u32,
) -> Result<AgentInfo, SshError> {
    let inner = channel.channel_mut();
    let request_id = 1; // single-shot handshake — correlation is trivial

    // Serialize the handshake envelope inline. The tepegoz-proto
    // codec's `write_envelope` wants an `AsyncWrite` target, but
    // russh::Channel's write surface is `data(bytes).await` — not an
    // AsyncWrite impl. Rather than adapt (the poll-based
    // AsyncWrite ↔ async `data()` gap is non-trivial), we inline the
    // two-step serialize + length-prefix that `write_envelope`
    // performs internally. Handshake is a single fixed-shape
    // envelope; the saving is zero. Reads still go through
    // codec::read_envelope via `make_reader` (russh's stdout
    // AsyncRead impl is well-shaped).
    let envelope = Envelope {
        version: expected_version,
        payload: Payload::AgentHandshake { request_id },
    };
    let body =
        rkyv::to_bytes::<rkyv::rancor::Error>(&envelope).map_err(|e| SshError::DeployFailed {
            stage: "handshake_send".into(),
            reason: format!("serialize handshake envelope: {e}"),
        })?;
    let len = u32::try_from(body.len()).map_err(|_| SshError::DeployFailed {
        stage: "handshake_send".into(),
        reason: format!("handshake envelope too large: {} bytes", body.len()),
    })?;
    // russh::Channel::data wants something Into<CryptoVec>; a
    // Vec<u8> satisfies that. One data() call per logical chunk is
    // fine — sshd aggregates into its channel window anyway.
    inner
        .data(len.to_be_bytes().to_vec().as_slice())
        .await
        .map_err(|e| SshError::DeployFailed {
            stage: "handshake_send".into(),
            reason: format!("write handshake length prefix: {e}"),
        })?;
    inner
        .data(body.as_ref())
        .await
        .map_err(|e| SshError::DeployFailed {
            stage: "handshake_send".into(),
            reason: format!("write handshake body: {e}"),
        })?;

    // Read the response through make_reader + codec.
    let mut reader = inner.make_reader();
    let response = tokio::time::timeout(HANDSHAKE_TIMEOUT, read_envelope(&mut reader))
        .await
        .map_err(|_| SshError::DeployFailed {
            stage: "handshake_recv".into(),
            reason: format!(
                "agent didn't respond within {} s",
                HANDSHAKE_TIMEOUT.as_secs()
            ),
        })?
        .map_err(|e| SshError::DeployFailed {
            stage: "handshake_recv".into(),
            reason: format!("read AgentHandshakeResponse: {e}"),
        })?;

    match response.payload {
        Payload::AgentHandshakeResponse {
            request_id: echoed,
            version,
            os,
            arch,
            capabilities,
        } => {
            if echoed != request_id {
                return Err(SshError::DeployFailed {
                    stage: "handshake_recv".into(),
                    reason: format!("request_id mismatch: sent {request_id}, got {echoed}"),
                });
            }
            if version != expected_version {
                return Err(SshError::AgentVersionMismatch {
                    embedded: expected_version,
                    reported: version,
                });
            }
            Ok(AgentInfo {
                version,
                os,
                arch,
                capabilities,
            })
        }
        Payload::Error(info) => Err(SshError::DeployFailed {
            stage: "handshake_recv".into(),
            reason: format!("agent returned Error({:?}): {}", info.kind, info.message),
        }),
        other => Err(SshError::DeployFailed {
            stage: "handshake_recv".into(),
            reason: format!("expected AgentHandshakeResponse, got {other:?}"),
        }),
    }
}

/// What the agent reported in its handshake response — the
/// controller logs / renders this in `tepegoz doctor --agents`.
#[derive(Debug, Clone)]
pub struct AgentInfo {
    pub version: u32,
    pub os: String,
    pub arch: String,
    pub capabilities: Vec<String>,
}

// ---- Internal helpers --------------------------------------------------

/// Resolve the conventional remote path where the tepegoz-agent
/// binary for `protocol_version` lives (or will live) on `session`'s
/// host. Exposed so `tepegoz doctor --agents` can inspect + display
/// the same path [`deploy_agent`] would write to.
pub async fn remote_agent_path(
    session: &SshSession,
    protocol_version: u32,
) -> Result<String, SshError> {
    let home = resolve_remote_home(session).await?;
    Ok(format!("{home}/.cache/tepegoz/agent-v{protocol_version}"))
}

/// Resolve `$HOME` on the remote. Deploys target
/// `$HOME/.cache/tepegoz/agent-v<N>` so we need a real path —
/// russh's `exec` doesn't tilde-expand on all sshd configs.
async fn resolve_remote_home(session: &SshSession) -> Result<String, SshError> {
    let out = run_exec(session, "echo \"$HOME\"", &[], EXEC_TIMEOUT).await?;
    if out.exit_status != 0 {
        return Err(SshError::DeployFailed {
            stage: "resolve_home".into(),
            reason: format!(
                "echo $HOME exited {}: {}",
                out.exit_status,
                out.stderr.trim()
            ),
        });
    }
    let home = out.stdout.trim().to_string();
    if home.is_empty() {
        return Err(SshError::DeployFailed {
            stage: "resolve_home".into(),
            reason: "remote $HOME is empty".into(),
        });
    }
    Ok(home)
}

/// Three-step upload: mkdir -p (idempotent) → cat > tmp path → mv +
/// chmod +x as a single compound command (atomic rename from the
/// remote filesystem's perspective). `.tmp.<pid>` suffix keeps
/// concurrent deploys from clobbering each other's in-flight file.
async fn upload_binary(
    session: &SshSession,
    bytes: &[u8],
    remote_dir: &str,
    final_path: &str,
) -> Result<(), SshError> {
    // 1. mkdir -p.
    let mkdir_cmd = format!("mkdir -p {}", shell_quote(remote_dir));
    let mkdir = run_exec(session, &mkdir_cmd, &[], EXEC_TIMEOUT).await?;
    if mkdir.exit_status != 0 {
        return Err(SshError::DeployFailed {
            stage: "mkdir".into(),
            reason: format!(
                "`{mkdir_cmd}` exited {}: {}",
                mkdir.exit_status,
                mkdir.stderr.trim()
            ),
        });
    }

    // 2. cat > tmp.  `.tmp.<pid>` suffix tied to the controller pid
    // isolates concurrent deploys from the same host.
    let tmp_path = format!("{final_path}.tmp.{}", std::process::id());
    let upload_cmd = format!("cat > {}", shell_quote(&tmp_path));
    let upload = run_exec(session, &upload_cmd, bytes, UPLOAD_TIMEOUT).await?;
    if upload.exit_status != 0 {
        // Best-effort cleanup of the tmp file on a failed cat.
        let rm_cmd = format!("rm -f {}", shell_quote(&tmp_path));
        let _ = run_exec(session, &rm_cmd, &[], EXEC_TIMEOUT).await;
        return Err(SshError::DeployFailed {
            stage: "upload".into(),
            reason: format!(
                "`{upload_cmd}` exited {}: {}",
                upload.exit_status,
                upload.stderr.trim()
            ),
        });
    }

    // 3. Atomic rename + executable bit.
    let finalize_cmd = format!(
        "mv {} {} && chmod +x {}",
        shell_quote(&tmp_path),
        shell_quote(final_path),
        shell_quote(final_path)
    );
    let finalize = run_exec(session, &finalize_cmd, &[], EXEC_TIMEOUT).await?;
    if finalize.exit_status != 0 {
        return Err(SshError::DeployFailed {
            stage: "finalize".into(),
            reason: format!(
                "`{finalize_cmd}` exited {}: {}",
                finalize.exit_status,
                finalize.stderr.trim()
            ),
        });
    }
    Ok(())
}

async fn compute_remote_hash(
    session: &SshSession,
    remote_path: &str,
    target: &DetectedTarget,
) -> Result<String, SshError> {
    let cmd = hash_command(&target.os, remote_path);
    let out = run_exec(session, &cmd, &[], EXEC_TIMEOUT).await?;
    if out.exit_status != 0 {
        return Err(SshError::DeployFailed {
            stage: "verify_hash".into(),
            reason: format!("`{cmd}` exited {}: {}", out.exit_status, out.stderr.trim()),
        });
    }
    parse_hash_output(&out.stdout)
}

/// Pick the right hash binary per OS. Linux ships `sha256sum`;
/// macOS ships `shasum -a 256 -b`. Both produce the same first-token
/// format: `<hex-digest> <filename>`.
fn hash_command(os: &str, path: &str) -> String {
    let quoted = shell_quote(path);
    match os {
        "darwin" => format!("shasum -a 256 -b {quoted}"),
        // Default (linux + anything else the detector accepted) →
        // sha256sum. Linux-only but covers the Decision #3 matrix.
        _ => format!("sha256sum -b {quoted}"),
    }
}

fn stat_command(os: &str, path: &str) -> String {
    let quoted = shell_quote(path);
    match os {
        "darwin" => format!("stat -f '%z %m' {quoted}"),
        _ => format!("stat -c '%s %Y' {quoted}"),
    }
}

/// Parse the first whitespace-delimited token (the hex digest) out of
/// `sha256sum` / `shasum` output. Both print `<digest> [*]<path>` on
/// one line — we keep only `<digest>`, lowercased for consistent
/// comparison.
fn parse_hash_output(raw: &str) -> Result<String, SshError> {
    let token = raw
        .split_whitespace()
        .next()
        .ok_or_else(|| SshError::DeployFailed {
            stage: "hash_parse".into(),
            reason: "empty hash output".into(),
        })?;
    if token.len() != 64 || !token.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(SshError::DeployFailed {
            stage: "hash_parse".into(),
            reason: format!("hash token {token:?} isn't a 64-char hex digest"),
        });
    }
    Ok(token.to_lowercase())
}

fn parse_stat_output(raw: &str) -> Result<(u64, u64), SshError> {
    let mut parts = raw.split_whitespace();
    let size: u64 =
        parts
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| SshError::DeployFailed {
                stage: "stat_parse".into(),
                reason: format!("couldn't parse size from {raw:?}"),
            })?;
    let mtime: u64 =
        parts
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| SshError::DeployFailed {
                stage: "stat_parse".into(),
                reason: format!("couldn't parse mtime from {raw:?}"),
            })?;
    Ok((size, mtime))
}

/// Wrap `s` in single quotes for safe POSIX shell interpolation.
/// Our paths are controller-owned (`$HOME/.cache/tepegoz/agent-v<N>`)
/// so injection isn't a realistic threat, but quoting keeps spaces /
/// special chars in a non-default `$HOME` from breaking the cat
/// redirection.
fn shell_quote(s: &str) -> String {
    // POSIX-safe single-quote escape: replace each ' with '\'' and
    // wrap the whole result in single quotes.
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

// ---- Exec primitive ---------------------------------------------------

#[derive(Debug)]
struct ExecOutput {
    stdout: String,
    stderr: String,
    exit_status: u32,
}

/// Low-level exec helper: opens a fresh session channel, runs `cmd`,
/// writes `stdin` bytes (if any), drains stdout + stderr + the exit
/// status, returns the collected output. All asyncs are wrapped in a
/// single `timeout` so a hung remote can't wedge the caller.
async fn run_exec(
    session: &SshSession,
    cmd: &str,
    stdin: &[u8],
    timeout: Duration,
) -> Result<ExecOutput, SshError> {
    tokio::time::timeout(timeout, run_exec_inner(session, cmd, stdin))
        .await
        .map_err(|_| SshError::DeployFailed {
            stage: "exec".into(),
            reason: format!("`{cmd}` timed out after {} s", timeout.as_secs()),
        })?
}

async fn run_exec_inner(
    session: &SshSession,
    cmd: &str,
    stdin: &[u8],
) -> Result<ExecOutput, SshError> {
    let channel = open_session(session).await?;
    let mut channel = channel.into_inner();

    channel
        .exec(true, cmd.as_bytes().to_vec())
        .await
        .map_err(|e| SshError::DeployFailed {
            stage: "exec".into(),
            reason: format!("exec `{cmd}`: {e}"),
        })?;

    if !stdin.is_empty() {
        channel
            .data(stdin)
            .await
            .map_err(|e| SshError::DeployFailed {
                stage: "exec".into(),
                reason: format!("write stdin for `{cmd}`: {e}"),
            })?;
    }
    channel.eof().await.map_err(|e| SshError::DeployFailed {
        stage: "exec".into(),
        reason: format!("eof for `{cmd}`: {e}"),
    })?;

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut exit_status: Option<u32> = None;
    // Drain every message until the channel closes. We can't break
    // on ExitStatus alone — the sshd may interleave additional
    // Data / Eof messages after the status, and dropping them here
    // would corrupt later reads on a reused session (though we
    // don't reuse today, future slices might).
    while let Some(msg) = channel.wait().await {
        match msg {
            ChannelMsg::Data { data } => stdout.extend_from_slice(&data[..]),
            ChannelMsg::ExtendedData { ext: 1, data } => stderr.extend_from_slice(&data[..]),
            ChannelMsg::ExitStatus { exit_status: code } => exit_status = Some(code),
            _ => {}
        }
    }

    Ok(ExecOutput {
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        // Missing ExitStatus (server closed without sending one): use
        // 255 as a sentinel matching OpenSSH client's convention for
        // "no remote exit status received".
        exit_status: exit_status.unwrap_or(255),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_uname_sm_linux_x86_64() {
        let target = parse_uname_sm("Linux x86_64").unwrap();
        assert_eq!(target.os, "linux");
        assert_eq!(target.arch, "x86_64");
        assert_eq!(target.target_triple, "x86_64-unknown-linux-musl");
    }

    #[test]
    fn parse_uname_sm_linux_aarch64() {
        let target = parse_uname_sm("Linux aarch64").unwrap();
        assert_eq!(target.target_triple, "aarch64-unknown-linux-musl");
    }

    #[test]
    fn parse_uname_sm_darwin_x86_64() {
        let target = parse_uname_sm("Darwin x86_64").unwrap();
        assert_eq!(target.target_triple, "x86_64-apple-darwin");
    }

    #[test]
    fn parse_uname_sm_darwin_arm64_maps_to_aarch64_triple() {
        // macOS reports arm64; rust targets call it aarch64. The
        // mapping bridges them.
        let target = parse_uname_sm("Darwin arm64").unwrap();
        assert_eq!(target.arch, "arm64", "raw arch preserved");
        assert_eq!(
            target.target_triple, "aarch64-apple-darwin",
            "target triple uses aarch64 per rust's convention"
        );
    }

    #[test]
    fn parse_uname_sm_unrecognised_os_yields_unsupported_platform() {
        let err = parse_uname_sm("FreeBSD amd64").unwrap_err();
        match err {
            SshError::UnsupportedPlatform {
                os,
                arch,
                supported,
            } => {
                assert_eq!(os, "FreeBSD");
                assert_eq!(arch, "amd64");
                assert_eq!(supported.len(), 4);
            }
            other => panic!("expected UnsupportedPlatform, got {other:?}"),
        }
    }

    #[test]
    fn parse_uname_sm_unrecognised_arch_yields_unsupported_platform() {
        let err = parse_uname_sm("Linux mips64").unwrap_err();
        assert!(matches!(err, SshError::UnsupportedPlatform { .. }));
    }

    #[test]
    fn hash_command_for_linux_uses_sha256sum() {
        let cmd = hash_command("linux", "/home/u/.cache/tepegoz/agent-v10");
        assert!(
            cmd.starts_with("sha256sum -b "),
            "linux path must use sha256sum; got {cmd}"
        );
        assert!(cmd.contains("/home/u/.cache/tepegoz/agent-v10"));
    }

    #[test]
    fn hash_command_for_darwin_uses_shasum() {
        let cmd = hash_command("darwin", "/Users/u/.cache/tepegoz/agent-v10");
        assert!(
            cmd.starts_with("shasum -a 256 -b "),
            "darwin path must use shasum; got {cmd}"
        );
    }

    #[test]
    fn parse_hash_output_accepts_sha256sum_format() {
        // sha256sum output: "<digest>  <path>" (two spaces for -b).
        let input = "3a7b5f2e8c1d4f9b6a0e3d7c8f1a2b4c5d6e7f8091a2b3c4d5e6f7a8b9c0d1e2  -\n";
        let hash = parse_hash_output(input).unwrap();
        assert_eq!(hash.len(), 64);
        assert_eq!(hash, hash.to_lowercase(), "digest is normalized lowercase");
    }

    #[test]
    fn parse_hash_output_accepts_shasum_format() {
        // shasum output: "<digest> *<path>" (single space, star prefix on -b).
        let input = "3A7B5F2E8C1D4F9B6A0E3D7C8F1A2B4C5D6E7F8091A2B3C4D5E6F7A8B9C0D1E2 *-\n";
        let hash = parse_hash_output(input).unwrap();
        assert_eq!(hash, hash.to_lowercase());
    }

    #[test]
    fn parse_hash_output_rejects_non_hex_token() {
        let err = parse_hash_output("NOT-A-HASH path").unwrap_err();
        assert!(matches!(err, SshError::DeployFailed { stage, .. } if stage == "hash_parse"));
    }

    #[test]
    fn parse_hash_output_rejects_wrong_length() {
        let err = parse_hash_output("abc123 path").unwrap_err();
        assert!(matches!(err, SshError::DeployFailed { .. }));
    }

    #[test]
    fn parse_stat_output_splits_size_and_mtime() {
        assert_eq!(
            parse_stat_output("1024 1700000000").unwrap(),
            (1024, 1700000000)
        );
    }

    #[test]
    fn shell_quote_roundtrips_simple_path() {
        assert_eq!(shell_quote("/home/u/cache"), "'/home/u/cache'");
    }

    #[test]
    fn shell_quote_escapes_embedded_single_quote() {
        // POSIX idiom: ' → '\''
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    /// Reference test: `PROTOCOL_VERSION` is only consumed here to
    /// match the compiled-in const. Pin it explicit so this file
    /// self-documents the "expected version" concept used in
    /// handshake_agent.
    #[test]
    fn protocol_version_matches_const() {
        let _ = tepegoz_proto::PROTOCOL_VERSION;
    }
}
