//! Spawns the real `opencode serve` binary in an isolated environment.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use reqwest::Client;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::config::TestConfig;

const STDERR_TAIL_LINES: usize = 200;
const URL_TIMEOUT: Duration = Duration::from_secs(10);
const READY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
struct Running {
    child: Child,
    url: String,
    stdout_task: JoinHandle<()>,
    stderr_task: JoinHandle<()>,
}

/// A running `opencode serve` child process with its isolated temp tree.
#[derive(Debug)]
pub struct ServerHarness {
    root: TempDir,
    binary: PathBuf,
    config: TestConfig,
    work: PathBuf,
    db: PathBuf,
    config_path: PathBuf,
    client: Client,
    stderr: Arc<Mutex<VecDeque<String>>>,
    running: Option<Running>,
}

impl ServerHarness {
    /// Create a temporary environment and start the server binary.
    ///
    /// # Errors
    ///
    /// Returns an error when the process cannot be spawned or does not become
    /// ready within five seconds.
    pub async fn start(binary: impl Into<PathBuf>, config: TestConfig) -> anyhow::Result<Self> {
        let mut harness = Self::prepare(binary, config)?;
        harness.launch().await?;
        harness.wait_ready().await?;
        Ok(harness)
    }

    /// Create the isolated temporary tree and configuration without spawning.
    ///
    /// Combine with [`Self::launch`] when the database must be prepared (for
    /// example seeded with a transcript) before the server opens it.
    ///
    /// # Errors
    ///
    /// Returns an error when the temp tree, config or HTTP client cannot be
    /// created.
    pub fn prepare(binary: impl Into<PathBuf>, config: TestConfig) -> anyhow::Result<Self> {
        let root = tempfile::tempdir().context("create harness temp dir")?;
        let work = root.path().join("work");
        let tmp = root.path().join("tmp");
        let home = root.path().join("home");
        let xdg = root.path().join("xdg");
        let program_data = root.path().join("programdata");
        for path in [&work, &tmp, &home, &xdg, &program_data] {
            std::fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
        }
        let config_path = root.path().join("opencode.json");
        config.write(&config_path).context("write test config")?;
        let db = root.path().join("opencode.db");

        let client = Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(30))
            .build()
            .context("build harness HTTP client")?;

        Ok(Self {
            root,
            binary: binary.into(),
            config,
            work,
            db,
            config_path,
            client,
            stderr: Arc::new(Mutex::new(VecDeque::new())),
            running: None,
        })
    }

    /// Spawn the server without waiting for it to answer.
    ///
    /// [`Self::wait_ready`] must be called before requests are issued.
    ///
    /// # Errors
    ///
    /// Returns an error when the process cannot be spawned or exits before
    /// printing its listen URL.
    pub async fn launch(&mut self) -> anyhow::Result<()> {
        self.spawn().await
    }

    /// Poll `/api/info` until it answers or the ready timeout elapses.
    ///
    /// # Errors
    ///
    /// Returns an error when the server does not become ready in time.
    pub async fn wait_ready(&self) -> anyhow::Result<()> {
        let running = self.running.as_ref().context("server is not running")?;
        let url = running.url.clone();
        self.await_ready(&url).await
    }

    /// OS process id of the running server, if any.
    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        self.running.as_ref().and_then(|running| running.child.id())
    }

    /// URL printed by the server (`http://127.0.0.1:<port>`).
    ///
    /// # Panics
    ///
    /// Panics when the server is not running.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.running.as_ref().expect("server is running").url
    }

    /// Directory passed via `--directory`.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.work
    }

    /// SQLite database path passed via `--db`.
    #[must_use]
    pub fn db_path(&self) -> &Path {
        &self.db
    }

    /// Temporary root shared by all harness paths.
    #[must_use]
    pub fn root(&self) -> &Path {
        self.root.path()
    }

    /// HTTP client configured without ambient proxies.
    #[must_use]
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Last lines written to stderr, for failure diagnostics.
    #[must_use]
    pub fn stderr_tail(&self) -> String {
        self.stderr
            .lock()
            .expect("stderr tail")
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Kill the child tree and wait for the process to exit.
    pub async fn stop(&mut self) {
        let Some(mut running) = self.running.take() else {
            return;
        };
        if let Some(pid) = running.child.id() {
            kill_tree(pid);
        }
        let _ = running.child.start_kill();
        let _ = running.child.wait().await;
        running.stdout_task.abort();
        running.stderr_task.abort();
    }

    /// Stop and start again against the same database and configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when the restarted server does not become ready.
    pub async fn restart(&mut self) -> anyhow::Result<()> {
        self.stop().await;
        self.spawn().await
    }

    async fn spawn(&mut self) -> anyhow::Result<()> {
        let db = self.db.to_string_lossy().into_owned();
        let work = self.work.to_string_lossy().into_owned();
        let mut command = Command::new(&self.binary);
        command
            .args([
                "serve",
                "--port",
                "0",
                "--db",
                db.as_str(),
                "--directory",
                work.as_str(),
            ])
            .current_dir(&self.work)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .env_clear();
        #[cfg(unix)]
        command.process_group(0);

        for key in [
            "PATH",
            "PATHEXT",
            "COMSPEC",
            "SystemRoot",
            "SystemDrive",
            "WINDIR",
            "USERNAME",
            "SHELL",
        ] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        command
            .env("HOME", self.root.path().join("home"))
            .env("USERPROFILE", self.root.path().join("home"))
            .env("XDG_CONFIG_HOME", self.root.path().join("xdg"))
            .env("ProgramData", self.root.path().join("programdata"))
            .env("TMP", self.root.path().join("tmp"))
            .env("TEMP", self.root.path().join("tmp"))
            .env("OPENCODE_CONFIG", &self.config_path)
            .env("OPENCODE_CONFIG_CONTENT", " ");
        for (key, value) in self.config.env() {
            command.env(key, value);
        }

        let mut child = command
            .spawn()
            .with_context(|| format!("spawn {}", self.binary.display()))?;
        let stdout = child.stdout.take().context("capture server stdout")?;
        let stderr = child.stderr.take().context("capture server stderr")?;

        let (url_tx, url_rx) = oneshot::channel();
        let stdout_task = tokio::spawn(read_stdout(stdout, url_tx));
        let stderr_task = tokio::spawn(read_stderr(stderr, Arc::clone(&self.stderr)));

        let url = match tokio::time::timeout(URL_TIMEOUT, url_rx).await {
            Ok(Ok(url)) => url,
            Ok(Err(_)) => bail!(
                "server exited before printing its URL\nstderr:\n{}",
                self.stderr_tail()
            ),
            Err(_) => bail!(
                "timed out waiting for the server URL\nstderr:\n{}",
                self.stderr_tail()
            ),
        };

        self.await_ready(&url).await?;

        self.running = Some(Running {
            child,
            url,
            stdout_task,
            stderr_task,
        });
        Ok(())
    }

    async fn await_ready(&self, url: &str) -> anyhow::Result<()> {
        let deadline = Instant::now() + READY_TIMEOUT;
        let info = format!("{url}/api/info");
        // A password-protected server answers `/api/info` with 401 until the
        // request carries a credential, so the readiness probe must send the
        // same Basic header the bench client sends. The credential comes from
        // the config that already drives the spawn, not from a second source.
        let authorization = self.config.env().iter().find_map(|(key, value)| {
            (key.as_str() == "OPENCODE_SERVER_PASSWORD" && !value.is_empty())
                .then(|| format!("Basic {}", BASE64.encode(format!("opencode:{value}"))))
        });
        loop {
            let mut request = self.client.get(&info);
            if let Some(authorization) = authorization.as_deref() {
                request = request.header(reqwest::header::AUTHORIZATION, authorization);
            }
            match request.send().await {
                Ok(response) if response.status().is_success() => return Ok(()),
                _ => {}
            }
            if Instant::now() >= deadline {
                bail!(
                    "server did not answer /api/info within {READY_TIMEOUT:?}\nstderr:\n{}",
                    self.stderr_tail()
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

impl Drop for ServerHarness {
    fn drop(&mut self) {
        if let Some(running) = self.running.take() {
            let pid = running.child.id();
            if let Some(pid) = pid {
                kill_tree(pid);
            }
            let Running {
                mut child,
                stdout_task,
                stderr_task,
                ..
            } = running;
            let _ = child.start_kill();
            let _ = child.try_wait();
            stdout_task.abort();
            stderr_task.abort();
        }
    }
}

async fn read_stdout(stdout: tokio::process::ChildStdout, url_tx: oneshot::Sender<String>) {
    let mut lines = BufReader::new(stdout).lines();
    let mut url_tx = Some(url_tx);
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(rest) = line.strip_prefix("opencode server listening on ")
            && let Some(tx) = url_tx.take()
        {
            let _ = tx.send(rest.trim().to_owned());
        }
    }
}

async fn read_stderr(stderr: tokio::process::ChildStderr, tail: Arc<Mutex<VecDeque<String>>>) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let mut tail = tail.lock().expect("stderr tail");
        if tail.len() == STDERR_TAIL_LINES {
            tail.pop_front();
        }
        tail.push_back(line);
    }
}

#[cfg(windows)]
fn kill_tree(pid: u32) {
    let taskkill = std::env::var_os("SystemRoot").map_or_else(
        || PathBuf::from("taskkill"),
        |root| PathBuf::from(root).join("System32").join("taskkill.exe"),
    );
    let _ = std::process::Command::new(taskkill)
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(not(windows))]
fn kill_tree(pid: u32) {
    if pid <= 1 {
        return;
    }
    let group = format!("-{pid}");
    let _ = std::process::Command::new("kill")
        .args(["-KILL", &group])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kill_tree_ignores_invalid_or_init_pids() {
        kill_tree(0);
        kill_tree(1);
    }

    #[cfg(unix)]
    #[test]
    fn kill_tree_terminates_process_group_children() {
        use std::os::unix::process::CommandExt as _;

        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep in own process group");
        let pid = child.id();
        kill_tree(pid);
        let status = child.wait().expect("reap killed process group leader");
        assert!(!status.success());
    }
}
