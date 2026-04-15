//! `cargo xtask demo-phase-5 {up,down}` — one-command runner for the
//! Slice 5e manual demo.
//!
//! Replaces the previous 60-line bash Prep block in
//! `docs/OPERATIONS.md` with a self-contained setup/teardown pair.
//! `up` provisions everything the 8-scenario manual demo needs (sshd
//! container, throwaway keypair, tepegoz config pointing at the
//! container, daemon running against isolated config/data dirs) and
//! blocks on Ctrl-C. `down` cleans up the fixture (idempotent —
//! safe to run after an interrupted `up`).
//!
//! Intentionally lean: stdlib, `tepegoz-proto::socket::default_socket_path`,
//! and `ctrlc`. No tokio, no bollard, no cargo-metadata — shelling
//! out to `docker`, `ssh-keygen`, and `cargo` is what a user does by
//! hand anyway, so the xtask stays faithful to the Operations-doc
//! flow it replaces.

use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

/// Stable container name — lets `down` remove the fixture even when
/// `up` crashed mid-flight without leaving us a handle. Matches the
/// `docs/OPERATIONS.md` convention (`tepegoz-<slice>-<service>`).
const CONTAINER_NAME: &str = "tepegoz-demo-phase-5-sshd";
const IMAGE: &str = "lscr.io/linuxserver/openssh-server:latest";
const SSHD_INTERNAL_PORT: u16 = 2222;
const TCP_READY_BUDGET: Duration = Duration::from_secs(30);
const SOCKET_READY_BUDGET: Duration = Duration::from_secs(5);
/// Pause after TCP comes up to let sshd finish its boot (install the
/// authorized_keys file + print the banner). Same value the opt-in
/// `ssh_smoke.rs` integration test uses.
const SSHD_BANNER_GRACE: Duration = Duration::from_millis(500);

/// Fixture locations. Stable path under `$TMPDIR` (via
/// `env::temp_dir()`) — never `mktemp -d` — so `down` can clean up
/// after a crashed `up`.
struct Paths {
    root: PathBuf,
    config_dir: PathBuf,
    data_dir: PathBuf,
    key_private: PathBuf,
    key_public: PathBuf,
    /// Daemon's PID written here by `up` so a detached `down`
    /// invocation can find and kill it without resorting to
    /// `pkill -f` (which would also kill the user's other daemons).
    pid_file: PathBuf,
    config_toml: PathBuf,
}

impl Paths {
    fn resolve() -> Self {
        let root = std::env::temp_dir().join("tepegoz-demo-phase-5");
        let config_dir = root.join("tepegoz-config");
        let data_dir = root.join("tepegoz-data");
        Self {
            key_private: root.join("id_ed25519"),
            key_public: root.join("id_ed25519.pub"),
            pid_file: root.join("daemon.pid"),
            config_toml: config_dir.join("config.toml"),
            config_dir,
            data_dir,
            root,
        }
    }

    fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("creating demo root at {}", self.root.display()))?;
        std::fs::create_dir_all(&self.config_dir)
            .with_context(|| format!("creating config dir at {}", self.config_dir.display()))?;
        std::fs::create_dir_all(&self.data_dir)
            .with_context(|| format!("creating data dir at {}", self.data_dir.display()))?;
        Ok(())
    }
}

pub(crate) fn up() -> Result<()> {
    preflight()?;
    let paths = Paths::resolve();
    paths.ensure_dirs()?;

    generate_keypair(&paths)?;
    remove_container_if_present();
    let port = start_sshd_container()?;
    write_config_toml(&paths, port)?;
    wait_for_tcp(port, TCP_READY_BUDGET)?;
    std::thread::sleep(SSHD_BANNER_GRACE);
    cargo_build()?;

    let mut daemon = spawn_daemon(&paths)?;
    let socket = tepegoz_proto::socket::default_socket_path();
    if let Err(e) = wait_for_socket(&socket, SOCKET_READY_BUDGET) {
        eprintln!("daemon never bound its socket — tearing down fixture");
        // Kill daemon; then teardown the container + filesystem state.
        let _ = daemon.kill();
        let _ = daemon.wait();
        remove_container_if_present();
        let _ = std::fs::remove_dir_all(&paths.root);
        return Err(e);
    }
    std::fs::write(&paths.pid_file, daemon.id().to_string())
        .with_context(|| format!("writing pid file to {}", paths.pid_file.display()))?;

    print_ready(&paths, port, &socket);

    wait_for_ctrl_c()?;

    teardown_with_child(&mut daemon, &paths);
    Ok(())
}

pub(crate) fn down() -> Result<()> {
    let paths = Paths::resolve();
    kill_daemon_via_pid_file(&paths);
    remove_container_if_present();
    if paths.root.exists() {
        std::fs::remove_dir_all(&paths.root)
            .with_context(|| format!("removing demo root at {}", paths.root.display()))?;
    }
    println!("Torn down.");
    Ok(())
}

// ─────────────────────── preflight ───────────────────────

fn preflight() -> Result<()> {
    ensure_on_path(
        "docker",
        "Docker is required — install Docker Desktop or Colima and retry",
    )?;
    ensure_on_path(
        "ssh-keygen",
        "ssh-keygen is required — it ships with OpenSSH (install `openssh-client` or similar)",
    )?;
    ensure_on_path(
        "cargo",
        "cargo is required — install Rust via https://rustup.rs and retry",
    )?;
    // Docker daemon must be reachable. `docker info` is the cheap probe.
    let status = Command::new("docker")
        .arg("info")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("running `docker info`")?;
    if !status.success() {
        bail!(
            "docker CLI is installed but the daemon isn't running — \
             start Docker Desktop / Colima / rancher-desktop and retry"
        );
    }
    Ok(())
}

fn ensure_on_path(binary: &str, hint: &str) -> Result<()> {
    // Spawnability check — not an exit-code check. macOS ssh-keygen
    // is OpenBSD-derived and rejects `--version` with a non-zero
    // exit + usage dump; the original preflight flagged a correctly
    // installed ssh-keygen as missing on every macOS run. `Command::
    // status()` only returns `Err(NotFound)` when the binary isn't
    // on PATH at all; any non-zero exit still lands in `Ok(_)` and
    // proves the binary exists.
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

// ─────────────────────── fixture setup ───────────────────────

fn generate_keypair(paths: &Paths) -> Result<()> {
    if paths.key_private.exists() && paths.key_public.exists() {
        // Idempotent: reuse whatever's there. If a prior `up` wrote
        // these files, they're fine to reuse for the next run.
        return Ok(());
    }
    let status = Command::new("ssh-keygen")
        .args([
            "-t",
            "ed25519",
            "-N",
            "",
            "-f",
            paths
                .key_private
                .to_str()
                .ok_or_else(|| anyhow!("keypath is not valid UTF-8"))?,
            "-q",
            "-C",
            "tepegoz-demo-phase-5",
        ])
        .status()
        .context("spawning ssh-keygen")?;
    if !status.success() {
        bail!("ssh-keygen failed to generate the demo keypair");
    }
    Ok(())
}

fn remove_container_if_present() {
    // Silent idempotent cleanup — don't propagate failure. The image
    // name is demo-fixture-specific so we never touch a user's other
    // containers.
    let _ = Command::new("docker")
        .args(["rm", "-f", CONTAINER_NAME])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn start_sshd_container() -> Result<u16> {
    // Re-read the pubkey each start so regenerated keypairs land in
    // the newly-spawned container via PUBLIC_KEY env.
    let paths = Paths::resolve();
    let pub_key = std::fs::read_to_string(&paths.key_public)
        .with_context(|| format!("reading {}", paths.key_public.display()))?
        .trim()
        .to_string();

    let out = Command::new("docker")
        .args([
            "run",
            "-d",
            "--name",
            CONTAINER_NAME,
            "-e",
            "PUID=1000",
            "-e",
            "PGID=1000",
            "-e",
            "USER_NAME=tepegoz",
            "-e",
            &format!("PUBLIC_KEY={pub_key}"),
            "-p",
            &format!("0:{SSHD_INTERNAL_PORT}"),
            IMAGE,
        ])
        .output()
        .context("spawning docker run")?;
    if !out.status.success() {
        bail!(
            "docker run failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    let port_out = Command::new("docker")
        .args(["port", CONTAINER_NAME, &format!("{SSHD_INTERNAL_PORT}/tcp")])
        .output()
        .context("reading container port")?;
    if !port_out.status.success() {
        bail!(
            "docker port failed: {}",
            String::from_utf8_lossy(&port_out.stderr).trim()
        );
    }
    let port_line = String::from_utf8_lossy(&port_out.stdout).trim().to_string();
    // `docker port` emits `0.0.0.0:12345` (IPv4) and/or `[::]:12345`
    // (IPv6) — one line per binding. Parse the first line's port.
    let first_line = port_line.lines().next().unwrap_or("");
    first_line
        .rsplit(':')
        .next()
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| {
            anyhow!("failed to parse host port from `docker port` output: {port_line:?}")
        })
}

fn write_config_toml(paths: &Paths, port: u16) -> Result<()> {
    let key_str = paths
        .key_private
        .to_str()
        .ok_or_else(|| anyhow!("keypath is not valid UTF-8"))?;
    let contents = format!(
        "[[ssh.hosts]]\n\
         alias = \"staging\"\n\
         hostname = \"127.0.0.1\"\n\
         port = {port}\n\
         user = \"tepegoz\"\n\
         identity_file = \"{key_str}\"\n"
    );
    std::fs::write(&paths.config_toml, contents)
        .with_context(|| format!("writing {}", paths.config_toml.display()))?;
    Ok(())
}

fn wait_for_tcp(port: u16, budget: Duration) -> Result<()> {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(
            &format!("127.0.0.1:{port}").parse().unwrap(),
            Duration::from_millis(500),
        )
        .is_ok()
        {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    bail!("sshd container did not accept TCP on 127.0.0.1:{port} within {budget:?}")
}

fn cargo_build() -> Result<()> {
    // Idempotent: if the workspace is already built, this returns
    // quickly. Surface cargo's own output (not silenced) so a real
    // compile error is visible to the user.
    let status = Command::new("cargo")
        .arg("build")
        .status()
        .context("spawning cargo build")?;
    if !status.success() {
        bail!("cargo build failed — fix the compile error and retry");
    }
    Ok(())
}

fn spawn_daemon(paths: &Paths) -> Result<Child> {
    // Binary path is `target/debug/tepegoz` relative to the workspace
    // root. The xtask runs from the workspace root under `cargo xtask`
    // so this relative path resolves correctly.
    let bin = PathBuf::from("target/debug/tepegoz");
    if !bin.exists() {
        bail!(
            "expected `{}` after cargo build — are you running `cargo xtask` from the workspace root?",
            bin.display()
        );
    }
    Command::new(&bin)
        .arg("daemon")
        .env("TEPEGOZ_CONFIG_DIR", &paths.config_dir)
        .env("TEPEGOZ_DATA_DIR", &paths.data_dir)
        // Inherit stderr so daemon tracing lands in the user's terminal
        // alongside the xtask output — keeps the demo legible without
        // juggling a log file.
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .context("spawning tepegoz daemon")
}

fn wait_for_socket(socket: &Path, budget: Duration) -> Result<()> {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if socket.exists() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    bail!(
        "daemon socket never appeared at {} within {budget:?} — \
         another tepegoz daemon may already be running",
        socket.display()
    )
}

fn print_ready(paths: &Paths, port: u16, socket: &Path) {
    println!();
    println!("sshd container: {CONTAINER_NAME} on 127.0.0.1:{port}");
    println!("tepegoz config: {}", paths.config_toml.display());
    println!("daemon socket:  {}", socket.display());
    println!("demo root:      {}", paths.root.display());
    println!();
    println!("Ready. Run 'tepegoz tui' in a new terminal.");
    println!();
    println!("(Ctrl-C here when done — it'll tear the fixture down cleanly.)");
}

// ─────────────────────── teardown ───────────────────────

fn wait_for_ctrl_c() -> Result<()> {
    // `ctrlc::set_handler` is the clean cross-platform way to block
    // on SIGINT (and SIGTERM with the `termination` feature) without
    // reaching for raw libc + sigaction. The channel means subsequent
    // signals after the first are no-ops — we don't want teardown to
    // race with itself.
    let (tx, rx) = std::sync::mpsc::channel();
    ctrlc::set_handler(move || {
        let _ = tx.send(());
    })
    .context("installing SIGINT handler")?;
    rx.recv().context("waiting for Ctrl-C")?;
    println!();
    println!("Tearing down…");
    Ok(())
}

fn teardown_with_child(daemon: &mut Child, paths: &Paths) {
    let _ = daemon.kill();
    let _ = daemon.wait();
    remove_container_if_present();
    let _ = std::fs::remove_dir_all(&paths.root);
    println!("Torn down.");
}

fn kill_daemon_via_pid_file(paths: &Paths) {
    let Ok(pid_str) = std::fs::read_to_string(&paths.pid_file) else {
        return; // no prior up, or up crashed before writing the pid file
    };
    let Ok(pid) = pid_str.trim().parse::<u32>() else {
        return;
    };
    // Send SIGTERM via `kill(1)` — portable across macOS + Linux
    // without pulling libc into the xtask. Ignore failure: the
    // process might already be gone.
    let _ = Command::new("kill")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    // Small grace to let SIGTERM land before the directory unlink
    // (daemon cleans up its socket on clean shutdown).
    std::thread::sleep(Duration::from_millis(300));
}
