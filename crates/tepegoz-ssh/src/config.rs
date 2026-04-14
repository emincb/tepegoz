//! SSH host discovery with Q2 precedence:
//!
//! 1. `config_dir()/tepegoz/config.toml` `[ssh.hosts]` table
//! 2. `TEPEGOZ_SSH_HOSTS=alias1,alias2,...` env (aliases looked up in
//!    `~/.ssh/config`, so you opt into a subset without editing ssh_config)
//! 3. `~/.ssh/config` — every concrete (non-wildcard) `Host` entry
//!
//! **First non-empty source wins, no merging.** The Fleet-tile footer
//! surfaces the resolved source so the user can tell at a glance whether
//! an override is active; `tepegoz doctor --ssh-hosts` (Slice 5b) dumps
//! the full list.
//!
//! Per-host resolution (Hostname, User, Port, IdentityFile, ProxyJump)
//! goes through `russh-config`, which honors the standard ssh_config(5)
//! merge rules + percent-token expansion for Hostname. Tepegöz does not
//! re-implement the parser.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::SshError;
use crate::paths;

/// A single remote host the user may connect to — `tepegoz connect <alias>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostEntry {
    /// Lookup name used by `tepegoz connect <alias>` and the Fleet tile.
    pub alias: String,
    /// DNS name or IP literally dialed.
    pub hostname: String,
    /// Remote user — resolved from `User` or falls back to the current
    /// username via `whoami`.
    pub user: String,
    /// TCP port. Defaults to 22.
    pub port: u16,
    /// `IdentityFile` entries in ssh_config order. Empty when the host
    /// relies on an SSH agent alone.
    pub identity_files: Vec<PathBuf>,
    /// `ProxyJump` alias. Carried forward unused in Phase 5 — v1.1
    /// reopens ProxyJump once the "chained SSH through another host"
    /// UX is actually needed.
    pub proxy_jump: Option<String>,
}

/// Where the host list came from — rendered in the Fleet-tile footer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostSource {
    TepegozConfig(PathBuf),
    Env,
    SshConfig(PathBuf),
    None,
}

impl HostSource {
    /// Short label for the Fleet-tile footer hint.
    pub fn label(&self) -> String {
        match self {
            HostSource::TepegozConfig(p) => format!("tepegoz config.toml ({})", p.display()),
            HostSource::Env => "TEPEGOZ_SSH_HOSTS env".to_string(),
            HostSource::SshConfig(p) => format!("ssh_config ({})", p.display()),
            HostSource::None => "(none)".to_string(),
        }
    }

    /// True when the source is an override (not the user's ssh_config).
    /// 5b uses this to decide whether to render the footer hint.
    pub fn is_override(&self) -> bool {
        matches!(self, HostSource::TepegozConfig(_) | HostSource::Env)
    }
}

#[derive(Debug, Clone)]
pub struct HostList {
    pub hosts: Vec<HostEntry>,
    pub source: HostSource,
}

impl HostList {
    /// Discover hosts per the Q2 precedence. Returns an empty list with
    /// `HostSource::None` when no source resolves — the Fleet tile
    /// handles this by rendering the first-run UX hint.
    pub fn discover() -> Result<Self, SshError> {
        if let Some(path) = paths::config_path() {
            if path.exists() {
                let hosts = parse_tepegoz_config(&path)?;
                if !hosts.is_empty() {
                    return Ok(Self {
                        hosts,
                        source: HostSource::TepegozConfig(path),
                    });
                }
            }
        }

        if let Ok(val) = std::env::var("TEPEGOZ_SSH_HOSTS") {
            let aliases: Vec<String> = val
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect();
            if !aliases.is_empty() {
                let hosts = parse_ssh_config_aliases(&aliases)?;
                return Ok(Self {
                    hosts,
                    source: HostSource::Env,
                });
            }
        }

        if let Some(path) = paths::ssh_config_path() {
            if path.exists() {
                let hosts = parse_all_ssh_config(&path)?;
                return Ok(Self {
                    hosts,
                    source: HostSource::SshConfig(path),
                });
            }
        }

        Ok(Self {
            hosts: Vec::new(),
            source: HostSource::None,
        })
    }

    /// Lookup a host by alias.
    pub fn get(&self, alias: &str) -> Option<&HostEntry> {
        self.hosts.iter().find(|h| h.alias == alias)
    }

    /// Number of aliases resolved.
    pub fn len(&self) -> usize {
        self.hosts.len()
    }

    /// True when no aliases were resolved.
    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty()
    }
}

// --- tepegoz config.toml -------------------------------------------------

#[derive(Debug, Deserialize)]
struct TepegozConfig {
    ssh: Option<TepegozSshTable>,
}

#[derive(Debug, Deserialize)]
struct TepegozSshTable {
    hosts: Option<Vec<TepegozHostEntry>>,
}

#[derive(Debug, Deserialize)]
struct TepegozHostEntry {
    alias: String,
    hostname: String,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    identity_file: Option<PathBuf>,
    #[serde(default)]
    proxy_jump: Option<String>,
}

fn parse_tepegoz_config(path: &Path) -> Result<Vec<HostEntry>, SshError> {
    let raw = std::fs::read_to_string(path)?;
    let cfg: TepegozConfig = toml::from_str(&raw).map_err(|e| SshError::TepegozConfig {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })?;
    let default_user = current_user();
    Ok(cfg
        .ssh
        .and_then(|s| s.hosts)
        .unwrap_or_default()
        .into_iter()
        .map(|h| HostEntry {
            alias: h.alias,
            hostname: h.hostname,
            user: h.user.unwrap_or_else(|| default_user.clone()),
            port: h.port.unwrap_or(22),
            identity_files: h.identity_file.map(|p| vec![p]).unwrap_or_default(),
            proxy_jump: h.proxy_jump,
        })
        .collect())
}

// --- ssh_config via russh-config ----------------------------------------

fn parse_ssh_config_aliases(aliases: &[String]) -> Result<Vec<HostEntry>, SshError> {
    let path = match paths::ssh_config_path() {
        Some(p) if p.exists() => p,
        _ => return Ok(Vec::new()),
    };
    let mut out = Vec::with_capacity(aliases.len());
    for alias in aliases {
        out.push(parse_one_alias(&path, alias)?);
    }
    Ok(out)
}

fn parse_all_ssh_config(path: &Path) -> Result<Vec<HostEntry>, SshError> {
    let raw = std::fs::read_to_string(path).map_err(|e| SshError::ConfigParse {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })?;
    // russh-config has no "list all hosts" API — it queries a single
    // alias. Walk the file to collect the concrete (non-wildcard) alias
    // set, then delegate each per-host resolution back to russh-config
    // so it handles the ssh_config(5) merge rules uniformly.
    let mut aliases: BTreeSet<String> = BTreeSet::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        // `Host` is case-insensitive in ssh_config.
        if trimmed.len() < 5
            || !trimmed[..4].eq_ignore_ascii_case("host")
            || !trimmed.as_bytes()[4].is_ascii_whitespace()
        {
            continue;
        }
        for token in trimmed[5..].split_whitespace() {
            // Skip wildcards — they produce no concrete alias.
            if token.contains('*') || token.contains('?') {
                continue;
            }
            // Negation prefix `!pattern` — skip outright (it's a
            // refinement, not a concrete alias).
            if token.starts_with('!') {
                continue;
            }
            if !token.is_empty() {
                aliases.insert(token.to_string());
            }
        }
    }
    let mut out = Vec::with_capacity(aliases.len());
    for alias in aliases {
        out.push(parse_one_alias(path, &alias)?);
    }
    Ok(out)
}

fn parse_one_alias(ssh_config_path: &Path, alias: &str) -> Result<HostEntry, SshError> {
    let cfg =
        russh_config::parse_path(ssh_config_path, alias).map_err(|e| SshError::ConfigParse {
            path: ssh_config_path.to_path_buf(),
            reason: e.to_string(),
        })?;
    let hostname = cfg.host().to_string();
    let user = cfg.user();
    let port = cfg.port();
    let identity_files = cfg.host_config.identity_file.unwrap_or_default();
    let proxy_jump = cfg.host_config.proxy_jump;
    Ok(HostEntry {
        alias: alias.to_string(),
        hostname,
        user,
        port,
        identity_files,
        proxy_jump,
    })
}

fn current_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "unknown".to_string())
}

// ------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn write_file(dir: &TempDir, name: &str, body: &str) -> PathBuf {
        let p = dir.path().join(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        p
    }

    #[test]
    fn ssh_config_walks_host_lines_and_resolves_each_alias() {
        let dir = TempDir::new().unwrap();
        let path = write_file(
            &dir,
            "config",
            "Host staging\n\
             \tHostname staging.internal\n\
             \tUser alice\n\
             \tPort 2222\n\
             \tIdentityFile ~/.ssh/id_ed25519_staging\n\
             \n\
             Host dev-eu bench-01\n\
             \tHostname %h.eu.dev\n\
             \tUser bob\n\
             \n\
             Host *\n\
             \tUser default-user\n",
        );
        let hosts = parse_all_ssh_config(&path).unwrap();
        assert_eq!(hosts.len(), 3, "three concrete aliases, wildcard skipped");

        let staging = hosts.iter().find(|h| h.alias == "staging").unwrap();
        assert_eq!(staging.hostname, "staging.internal");
        assert_eq!(staging.user, "alice");
        assert_eq!(staging.port, 2222);
        assert_eq!(staging.identity_files.len(), 1);

        let dev = hosts.iter().find(|h| h.alias == "dev-eu").unwrap();
        assert_eq!(dev.user, "bob");
        // russh-config expands %h → alias at parse time.
        assert!(dev.hostname.contains("eu.dev"));

        let bench = hosts.iter().find(|h| h.alias == "bench-01").unwrap();
        assert_eq!(bench.user, "bob", "second alias on same Host line inherits");
    }

    #[test]
    fn ssh_config_skips_wildcards_and_negation_and_comments() {
        let dir = TempDir::new().unwrap();
        let path = write_file(
            &dir,
            "config",
            "# This is a comment\n\
             Host *.example.com\n\
             \tUser wild\n\
             \n\
             Host !restricted\n\
             \tUser neg\n\
             \n\
             Host real-box\n\
             \tHostname real.example.com\n",
        );
        let hosts = parse_all_ssh_config(&path).unwrap();
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].alias, "real-box");
    }

    #[test]
    fn ssh_config_empty_file_yields_empty_host_list() {
        let dir = TempDir::new().unwrap();
        let path = write_file(&dir, "config", "");
        let hosts = parse_all_ssh_config(&path).unwrap();
        assert!(hosts.is_empty());
    }

    #[test]
    fn ssh_config_case_insensitive_host_keyword() {
        let dir = TempDir::new().unwrap();
        let path = write_file(
            &dir,
            "config",
            "HOST upper\n\
             \tHostname upper.box\n\
             host lower\n\
             \tHostname lower.box\n\
             HoSt mixed\n\
             \tHostname mixed.box\n",
        );
        let hosts = parse_all_ssh_config(&path).unwrap();
        let mut aliases: Vec<_> = hosts.iter().map(|h| h.alias.as_str()).collect();
        aliases.sort();
        assert_eq!(aliases, vec!["lower", "mixed", "upper"]);
    }

    #[test]
    fn tepegoz_config_replaces_ssh_config_when_ssh_hosts_table_populated() {
        let dir = TempDir::new().unwrap();
        let path = write_file(
            &dir,
            "tepegoz.toml",
            r#"
[[ssh.hosts]]
alias = "prod-api"
hostname = "10.0.0.5"
user = "deploy"
port = 22

[[ssh.hosts]]
alias = "prod-db"
hostname = "10.0.0.6"
port = 5432
"#,
        );
        let hosts = parse_tepegoz_config(&path).unwrap();
        assert_eq!(hosts.len(), 2);
        let api = hosts.iter().find(|h| h.alias == "prod-api").unwrap();
        assert_eq!(api.hostname, "10.0.0.5");
        assert_eq!(api.user, "deploy");
        assert_eq!(api.port, 22);
        let db = hosts.iter().find(|h| h.alias == "prod-db").unwrap();
        assert_eq!(db.port, 5432);
    }

    #[test]
    fn tepegoz_config_empty_ssh_section_is_not_an_error() {
        let dir = TempDir::new().unwrap();
        let path = write_file(&dir, "tepegoz.toml", "# no ssh section\n");
        let hosts = parse_tepegoz_config(&path).unwrap();
        assert!(hosts.is_empty());
    }

    #[test]
    fn tepegoz_config_malformed_toml_surfaces_parse_error() {
        let dir = TempDir::new().unwrap();
        let path = write_file(&dir, "tepegoz.toml", "this is !!! not toml\n[[unbalanced");
        let err = parse_tepegoz_config(&path).unwrap_err();
        match err {
            SshError::TepegozConfig { reason, .. } => {
                assert!(!reason.is_empty(), "parse error must carry a reason");
            }
            other => panic!("expected TepegozConfig error, got {other:?}"),
        }
    }

    #[test]
    fn host_source_label_is_stable() {
        assert_eq!(HostSource::Env.label(), "TEPEGOZ_SSH_HOSTS env");
        assert_eq!(HostSource::None.label(), "(none)");
        assert!(
            HostSource::SshConfig(PathBuf::from("/home/u/.ssh/config"))
                .label()
                .contains("ssh_config")
        );
        assert!(
            HostSource::TepegozConfig(PathBuf::from("/x/config.toml"))
                .label()
                .contains("config.toml")
        );
    }

    #[test]
    fn host_source_is_override_flags_tepegoz_and_env() {
        assert!(HostSource::Env.is_override());
        assert!(HostSource::TepegozConfig(PathBuf::from("/x")).is_override());
        assert!(!HostSource::SshConfig(PathBuf::from("/x")).is_override());
        assert!(!HostSource::None.is_override());
    }
}
