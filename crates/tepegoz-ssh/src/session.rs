//! High-level SSH session: connect + auth + open channel.
//!
//! [`connect_host`] resolves a [`HostEntry`] by alias, dials the remote,
//! verifies the server key with TOFU against
//! [`KnownHostsStore`](crate::known_hosts::KnownHostsStore), authenticates
//! via SSH agent → IdentityFile(s) (first success wins), and returns a
//! live [`SshSession`].
//!
//! [`open_session`] opens one SSH "session" channel on the connection.
//! 5a exposes the russh channel directly; Slice 5d layers pty/shell on
//! top without wrapping the channel further.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::client::{self, AuthResult, Handle, Handler};
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::AgentClient;
use russh::keys::{PrivateKeyWithHashAlg, PublicKey, load_secret_key};

use crate::config::HostList;
use crate::error::SshError;
use crate::known_hosts::{HostKeyVerdict, KnownHostsStore};

/// A connected + authenticated SSH session.
pub struct SshSession {
    handle: Handle<TofuHandler>,
    alias: String,
    hostname: String,
    port: u16,
}

impl SshSession {
    pub fn alias(&self) -> &str {
        &self.alias
    }

    pub fn hostname(&self) -> &str {
        &self.hostname
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Underlying russh handle — 5c's keepalive + 5d's pty requests
    /// reach through this.
    pub fn handle(&self) -> &Handle<TofuHandler> {
        &self.handle
    }

    /// Close the connection politely. Dropping the session also closes,
    /// so callers can skip this; use it when you want a post-close
    /// error signal.
    pub async fn disconnect(self) -> Result<(), SshError> {
        self.handle
            .disconnect(russh::Disconnect::ByApplication, "tepegoz disconnect", "en")
            .await
            .map_err(|e| SshError::ConnectFailed {
                alias: self.alias,
                hostname: self.hostname,
                port: self.port,
                reason: format!("disconnect: {e}"),
            })
    }
}

/// A live SSH channel. 5a exposes the inner russh channel; 5d wraps
/// this with pty request + shell/exec in the daemon-side `RemotePane`.
pub struct SshChannel {
    channel: russh::Channel<client::Msg>,
}

impl SshChannel {
    /// Consume and return the wrapped russh channel — callers that need
    /// the full surface (pty request, exec, data) work directly with it.
    pub fn into_inner(self) -> russh::Channel<client::Msg> {
        self.channel
    }

    /// Mutable access to the underlying channel without consuming the
    /// wrapper. Named `channel_mut` rather than `as_mut` to avoid the
    /// `AsMut` trait ambiguity — callers in 5d need the specific russh
    /// type, not a generic trait bound.
    pub fn channel_mut(&mut self) -> &mut russh::Channel<client::Msg> {
        &mut self.channel
    }
}

/// Connect to the host identified by `alias`.
///
/// Flow:
/// 1. Look up `alias` in `hosts` — `UnknownAlias` if absent.
/// 2. `russh::client::connect(...)`; the [`TofuHandler`] verifies the
///    server's host key against `known_hosts` during key exchange.
/// 3. Authenticate: SSH agent (via `$SSH_AUTH_SOCK`) → each ssh_config
///    `IdentityFile` in order. First success returns.
/// 4. Return the live [`SshSession`].
pub async fn connect_host(
    alias: &str,
    hosts: &HostList,
    known_hosts: &KnownHostsStore,
) -> Result<SshSession, SshError> {
    let entry = hosts.get(alias).ok_or_else(|| SshError::UnknownAlias {
        alias: alias.to_string(),
        source_label: hosts.source.label(),
    })?;

    let outcome = Arc::new(Mutex::new(TofuOutcome::default()));
    let handler = TofuHandler {
        store: known_hosts.clone(),
        hostname: entry.hostname.clone(),
        port: entry.port,
        outcome: Arc::clone(&outcome),
    };

    let config = Arc::new(client::Config {
        inactivity_timeout: Some(Duration::from_secs(300)),
        ..Default::default()
    });

    let handle = match client::connect(config, (entry.hostname.as_str(), entry.port), handler).await
    {
        Ok(h) => h,
        Err(e) => {
            // Translate connect-path TOFU outcomes into structured errors.
            let mut state = outcome.lock().expect("tofu outcome mutex poisoned");
            if let Some(HostKeyVerdict::Mismatch { stored_line }) = state.verdict {
                return Err(SshError::HostKeyMismatch {
                    alias: alias.to_string(),
                    hostname: entry.hostname.clone(),
                    port: entry.port,
                    path: known_hosts.path().to_path_buf(),
                    line: stored_line,
                });
            }
            if let Some(err) = state.trust_error.take() {
                return Err(err);
            }
            return Err(SshError::ConnectFailed {
                alias: alias.to_string(),
                hostname: entry.hostname.clone(),
                port: entry.port,
                reason: e.to_string(),
            });
        }
    };

    let mut handle = handle;
    authenticate(
        &mut handle,
        &entry.user,
        &entry.identity_files,
        alias,
        &entry.hostname,
        entry.port,
    )
    .await?;

    Ok(SshSession {
        handle,
        alias: alias.to_string(),
        hostname: entry.hostname.clone(),
        port: entry.port,
    })
}

/// Open a "session" channel on the connected session.
pub async fn open_session(session: &SshSession) -> Result<SshChannel, SshError> {
    let channel =
        session
            .handle
            .channel_open_session()
            .await
            .map_err(|e| SshError::ConnectFailed {
                alias: session.alias.clone(),
                hostname: session.hostname.clone(),
                port: session.port,
                reason: format!("channel_open_session: {e}"),
            })?;
    Ok(SshChannel { channel })
}

// --- auth ---------------------------------------------------------------

async fn authenticate(
    handle: &mut Handle<TofuHandler>,
    user: &str,
    identity_files: &[String],
    alias: &str,
    hostname: &str,
    port: u16,
) -> Result<(), SshError> {
    let mut attempts: Vec<String> = Vec::new();

    // Phase 1 — SSH agent via $SSH_AUTH_SOCK.
    match AgentClient::connect_env().await {
        Ok(mut agent) => match agent.request_identities().await {
            Ok(ids) if ids.is_empty() => {
                attempts.push("ssh-agent connected but holds no identities".into());
            }
            Ok(ids) => {
                let total = ids.len();
                let cert_count = ids
                    .iter()
                    .filter(|id| matches!(id, AgentIdentity::Certificate { .. }))
                    .count();
                let pk_count = total - cert_count;
                attempts.push(format!(
                    "ssh-agent: {pk_count} public-key identity(ies) attempted"
                ));
                if cert_count > 0 {
                    // Phase 5 skips certificate auth — improve visibility
                    // so a user relying on SSH certs sees a legible reason
                    // rather than a silent skip. v1.1 reopen if asked.
                    attempts.push(format!(
                        "{cert_count} certificate identity(ies) in agent \
                         skipped (SSH certificates not supported in Phase 5)"
                    ));
                }
                for id in &ids {
                    let pk = match id {
                        AgentIdentity::PublicKey { key, .. } => key.clone(),
                        AgentIdentity::Certificate { .. } => continue,
                    };
                    match handle
                        .authenticate_publickey_with(user, pk, None, &mut agent)
                        .await
                    {
                        Ok(AuthResult::Success) => return Ok(()),
                        Ok(AuthResult::Failure { .. }) => {
                            attempts.push("agent identity rejected".into());
                        }
                        Err(e) => {
                            attempts.push(format!("agent sign error: {e:?}"));
                            break;
                        }
                    }
                }
            }
            Err(e) => attempts.push(format!("ssh-agent request_identities failed: {e}")),
        },
        Err(russh::keys::Error::EnvVar(_)) => {
            attempts.push("SSH_AUTH_SOCK not set; skipping ssh-agent".into());
        }
        Err(e) => attempts.push(format!("ssh-agent connect failed: {e}")),
    }

    // Phase 2 — IdentityFile(s) from ssh_config, in declaration order.
    for path_str in identity_files {
        let path = PathBuf::from(path_str);
        match load_secret_key(&path, None) {
            Ok(key) => {
                let kw = PrivateKeyWithHashAlg::new(Arc::new(key), None);
                match handle.authenticate_publickey(user, kw).await {
                    Ok(AuthResult::Success) => return Ok(()),
                    Ok(AuthResult::Failure { .. }) => {
                        attempts.push(format!("IdentityFile {path_str} rejected"));
                    }
                    Err(e) => attempts.push(format!("IdentityFile {path_str} error: {e}")),
                }
            }
            Err(russh::keys::Error::KeyIsEncrypted) => {
                // Per Q4 CTO addition: passphrase-protected key with no
                // agent unlocking it must surface verbatim — never hang.
                attempts.push(format!(
                    "IdentityFile {path_str} is passphrase-protected and \
                     no SSH agent unlocked it"
                ));
            }
            Err(e) => attempts.push(format!("IdentityFile {path_str} load error: {e}")),
        }
    }

    if identity_files.is_empty() {
        attempts.push("no IdentityFile declared in ssh_config".into());
    }

    Err(SshError::AuthFailed {
        alias: alias.to_string(),
        user: user.to_string(),
        hostname: hostname.to_string(),
        port,
        reason: attempts.join("; "),
    })
}

// --- TOFU handler -------------------------------------------------------

#[derive(Debug, Default)]
struct TofuOutcome {
    verdict: Option<HostKeyVerdict>,
    trust_error: Option<SshError>,
}

/// Russh `client::Handler` that verifies the server's host key against
/// a [`KnownHostsStore`] during key exchange. Outcome is stashed in
/// `Arc<Mutex<TofuOutcome>>` so the caller can distinguish
/// "connection failed generically" from "mismatch rejected by TOFU".
pub struct TofuHandler {
    store: KnownHostsStore,
    hostname: String,
    port: u16,
    outcome: Arc<Mutex<TofuOutcome>>,
}

impl Handler for TofuHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> Result<bool, russh::Error> {
        let verdict = self
            .store
            .check(&self.hostname, self.port, server_public_key);
        let mut state = self.outcome.lock().expect("tofu outcome mutex poisoned");
        match verdict {
            Ok(HostKeyVerdict::Trusted) => {
                state.verdict = Some(HostKeyVerdict::Trusted);
                Ok(true)
            }
            Ok(HostKeyVerdict::Unknown) => {
                if let Err(e) = self
                    .store
                    .trust(&self.hostname, self.port, server_public_key)
                {
                    state.trust_error = Some(e);
                    return Ok(false);
                }
                state.verdict = Some(HostKeyVerdict::Unknown);
                Ok(true)
            }
            Ok(HostKeyVerdict::Mismatch { stored_line }) => {
                state.verdict = Some(HostKeyVerdict::Mismatch { stored_line });
                Ok(false)
            }
            Err(e) => {
                state.trust_error = Some(e);
                Ok(false)
            }
        }
    }
}
