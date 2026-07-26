use crate::config::{ForgeConfig, ServerConfig, resolve_server_env};
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Child;
use tokio::process::Command;
use tokio::sync::{Mutex, watch};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

const LOG_BUFFER_LINES: usize = 1000;

#[derive(Debug)]
pub struct Supervisor {
    config: ForgeConfig,
    handles: HashMap<String, ServerHandle>,
    tasks: JoinSet<(String, ServerResult)>,
    shutdown: CancellationToken,
    state_path: PathBuf,
    started_at_secs: u64,
}

#[derive(Debug, Clone)]
pub struct ServerHandle {
    pub health_tx: watch::Sender<ServerHealth>,
    pub health_rx: watch::Receiver<ServerHealth>,
    pub log_buffer: Arc<Mutex<VecDeque<LogLine>>>,
    pub log_file: PathBuf,
    pub last_pid: Arc<Mutex<Option<u32>>>,
}

#[derive(Debug, Clone, Serialize)]
pub enum ServerHealth {
    Starting,
    Running {
        pid: u32,
        uptime_secs: u64,
        restarts: u32,
    },
    Degraded {
        restarts: u32,
        last_error: String,
    },
    Stopped,
}

#[derive(Debug, Clone, Serialize)]
pub enum LogStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone)]
pub struct LogLine {
    pub timestamp_secs: u64,
    pub stream: LogStream,
    pub content: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PersistentState {
    pub started_at_secs: u64,
    pub servers: HashMap<String, ServerState>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ServerState {
    pub status: String,
    pub pid: Option<u32>,
    pub uptime_secs: Option<u64>,
    pub restarts: u32,
    pub last_error: Option<String>,
    /// Remote MCP endpoint (HTTP/SSE servers have no local PID).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
}

#[derive(Debug)]
pub enum ServerResult {
    CleanExit,
    MaxRestartsExceeded,
    SpawnError(anyhow::Error),
    Shutdown,
    UserStopped,
}

impl Supervisor {
    pub fn new(config: ForgeConfig) -> Result<Self> {
        let home = forge_home_dir()?;
        let log_dir = home.join("logs");
        fs::create_dir_all(&log_dir).with_context(|| {
            format!(
                "failed to create forge log directory '{}'",
                log_dir.display()
            )
        })?;

        let state_path = home.join("state.json");
        let started_at_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())?;

        let mut handles = HashMap::new();
        for name in config.server.keys() {
            let (health_tx, health_rx) = watch::channel(ServerHealth::Stopped);
            let log_file = log_dir.join(format!("{}.log", name));
            handles.insert(
                name.clone(),
                ServerHandle {
                    health_tx,
                    health_rx,
                    log_buffer: Arc::new(Mutex::new(VecDeque::new())),
                    log_file,
                    last_pid: Arc::new(Mutex::new(None)),
                },
            );
        }

        Ok(Self {
            config,
            handles,
            tasks: JoinSet::new(),
            shutdown: CancellationToken::new(),
            state_path,
            started_at_secs,
        })
    }

    pub async fn start_all(&mut self) -> Result<()> {
        let handles = self.handles.clone();
        for (name, server_config) in self.config.server.clone() {
            self.spawn_server(name, server_config).await?;
        }

        write_state_file(&self.state_path, self.started_at_secs, &handles)?;
        let state_path = self.state_path.clone();
        let started_at_secs = self.started_at_secs;
        let state_handles = handles.clone();
        let state_task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                interval.tick().await;
                let _ = write_state_file(&state_path, started_at_secs, &state_handles);
            }
        });

        println!("Starting {} servers...", self.handles.len());
        println!("Press Ctrl+C to stop.");

        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    println!("Shutting down...");
                    self.shutdown.cancel();
                    break;
                }
                _ = self.shutdown.cancelled() => {
                    break;
                }
                result = self.tasks.join_next() => {
                    match result {
                        None => break, // all tasks have completed
                        Some(Ok((name, ServerResult::SpawnError(e)))) => {
                            eprintln!("error: server '{}' failed to start: {}", name, e);
                            self.shutdown.cancel();
                        }
                        Some(Ok((name, ServerResult::MaxRestartsExceeded))) => {
                            eprintln!("error: server '{}' exceeded max restart attempts and exited", name);
                            self.shutdown.cancel();
                        }
                        Some(Ok((name, ServerResult::CleanExit))) => {
                            println!("server '{}' exited cleanly", name);
                            // Do not cancel: other servers may still be running.
                        }
                        Some(Ok((_, ServerResult::Shutdown | ServerResult::UserStopped))) => {}
                        Some(Err(e)) => {
                            eprintln!("task join error: {}", e);
                        }
                    }
                }
            }
        }

        while self.tasks.join_next().await.is_some() {}
        state_task.abort();
        Ok(())
    }

    async fn spawn_server(&mut self, name: String, config: ServerConfig) -> Result<()> {
        let handle = self
            .handles
            .get(&name)
            .cloned()
            .ok_or_else(|| anyhow!("missing server handle for '{}'", name))?;

        let health_tx = handle.health_tx.clone();
        let log_buffer = handle.log_buffer.clone();
        let log_file = handle.log_file.clone();
        let last_pid = handle.last_pid.clone();
        let shutdown = self.shutdown.clone();

        self.tasks.spawn(async move {
            let result = run_server_loop(
                name.clone(),
                config,
                health_tx,
                log_buffer,
                log_file,
                last_pid,
                shutdown,
            )
            .await;
            (name, result)
        });

        Ok(())
    }
}

async fn run_server_loop(
    name: String,
    config: ServerConfig,
    health_tx: watch::Sender<ServerHealth>,
    log_buffer: Arc<Mutex<VecDeque<LogLine>>>,
    log_file: PathBuf,
    last_pid: Arc<Mutex<Option<u32>>>,
    shutdown: CancellationToken,
) -> ServerResult {
    let mut restart_count = 0u32;
    let max_restarts = config.max_restarts.unwrap_or(5);
    let max_backoff = Duration::from_secs(30);

    loop {
        if is_stop_requested(&name) {
            let _ = health_tx.send_replace(ServerHealth::Stopped);
            return ServerResult::UserStopped;
        }

        let (mut child, pid, capture_task) = match try_spawn(
            &name,
            &config,
            restart_count,
            &health_tx,
            &log_buffer,
            &log_file,
            &last_pid,
        )
        .await
        {
            Ok(t) => t,
            Err(r) => return r,
        };

        let start = Instant::now();
        loop {
            tokio::select! {
                status = child.wait() => {
                    capture_task.abort();
                    match status {
                        Ok(exit) if exit.success() => {
                            let _ = health_tx.send_replace(ServerHealth::Stopped);
                            return ServerResult::CleanExit;
                        }
                        Ok(exit) => {
                            restart_count += 1;
                            let error = format!("exit code: {:?}", exit.code());
                            let _ = health_tx.send_replace(ServerHealth::Degraded {
                                restarts: restart_count,
                                last_error: error,
                            });
                            if restart_count >= max_restarts {
                                return ServerResult::MaxRestartsExceeded;
                            }
                            let backoff_secs =
                                (1u64 << restart_count.min(5)).min(max_backoff.as_secs());
                            tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                            break; // restart outer loop
                        }
                        Err(err) => {
                            let _ = health_tx.send_replace(ServerHealth::Degraded {
                                restarts: restart_count,
                                last_error: err.to_string(),
                            });
                            return ServerResult::SpawnError(err.into());
                        }
                    }
                }
                _ = shutdown.cancelled() => {
                    capture_task.abort();
                    let _ = child.kill().await;
                    let _ = health_tx.send_replace(ServerHealth::Stopped);
                    return ServerResult::Shutdown;
                }
                _ = tokio::time::sleep(Duration::from_secs(1)) => {
                    let _ = health_tx.send_replace(ServerHealth::Running {
                        pid,
                        uptime_secs: start.elapsed().as_secs(),
                        restarts: restart_count,
                    });
                }
            }
        }
    }
}

fn is_stop_requested(name: &str) -> bool {
    match user_stop_marker_path(name) {
        Ok(marker) if marker.exists() => {
            let _ = fs::remove_file(&marker);
            true
        }
        _ => false,
    }
}

async fn try_spawn(
    name: &str,
    config: &ServerConfig,
    restart_count: u32,
    health_tx: &watch::Sender<ServerHealth>,
    log_buffer: &Arc<Mutex<VecDeque<LogLine>>>,
    log_file: &Path,
    last_pid: &Arc<Mutex<Option<u32>>>,
) -> Result<(Child, u32, tokio::task::JoinHandle<()>), ServerResult> {
    let _ = health_tx.send_replace(ServerHealth::Starting);

    let env_vars = match resolve_server_env(config).await {
        Ok(vars) => vars,
        Err(err) => {
            let _ = health_tx.send_replace(ServerHealth::Degraded {
                restarts: restart_count,
                last_error: err.to_string(),
            });
            return Err(ServerResult::SpawnError(err));
        }
    };

    let parts = config.cmd_parts();
    if parts.is_empty() {
        let err = anyhow!("server '{}' command is empty", name);
        let _ = health_tx.send_replace(ServerHealth::Degraded {
            restarts: restart_count,
            last_error: err.to_string(),
        });
        return Err(ServerResult::SpawnError(err));
    }

    let mut child = match Command::new(&parts[0])
        .args(&parts[1..])
        .envs(env_vars)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(err) => {
            let _ = health_tx.send_replace(ServerHealth::Degraded {
                restarts: restart_count,
                last_error: err.to_string(),
            });
            return Err(ServerResult::SpawnError(err.into()));
        }
    };

    let pid = match child.id() {
        Some(p) => p,
        None => {
            let err = anyhow!(
                "failed to get PID for server '{}': process may have already exited",
                name
            );
            let _ = health_tx.send_replace(ServerHealth::Degraded {
                restarts: restart_count,
                last_error: err.to_string(),
            });
            return Err(ServerResult::SpawnError(err));
        }
    };

    *last_pid.lock().await = Some(pid);
    let _ = health_tx.send_replace(ServerHealth::Running {
        pid,
        uptime_secs: 0,
        restarts: restart_count,
    });

    let (stdout, stderr) = match (child.stdout.take(), child.stderr.take()) {
        (Some(out), Some(err)) => (out, err),
        _ => {
            let err = anyhow!("failed to capture stdout/stderr for '{}'", name);
            let _ = health_tx.send_replace(ServerHealth::Degraded {
                restarts: restart_count,
                last_error: err.to_string(),
            });
            // Kill the already-spawned child to avoid orphaned processes.
            let _ = child.kill().await;
            return Err(ServerResult::SpawnError(err));
        }
    };

    let buf = log_buffer.clone();
    let lf = log_file.to_path_buf();
    let capture = tokio::spawn(async move {
        let _ = capture_output(stdout, stderr, buf, lf).await;
    });

    Ok((child, pid, capture))
}

async fn capture_output(
    stdout: tokio::process::ChildStdout,
    stderr: tokio::process::ChildStderr,
    buffer: Arc<Mutex<VecDeque<LogLine>>>,
    log_file: PathBuf,
) -> Result<()> {
    let mut output_file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_file)
        .await
        .with_context(|| format!("failed to open log file '{}'", log_file.display()))?;

    let mut stdout_reader = BufReader::new(stdout).lines();
    let mut stderr_reader = BufReader::new(stderr).lines();
    let mut stdout_done = false;
    let mut stderr_done = false;

    loop {
        if stdout_done && stderr_done {
            break;
        }
        tokio::select! {
            result = stdout_reader.next_line(), if !stdout_done => {
                match result {
                    Ok(Some(line)) => {
                        push_log(&buffer, LogStream::Stdout, line.clone()).await;
                        append_log(&mut output_file, LogStream::Stdout, line).await?;
                    }
                    Ok(None) => {
                        stdout_done = true;
                    }
                    Err(e) => {
                        tracing::warn!("error reading stdout from child process: {}", e);
                        stdout_done = true;
                    }
                }
            }
            result = stderr_reader.next_line(), if !stderr_done => {
                match result {
                    Ok(Some(line)) => {
                        push_log(&buffer, LogStream::Stderr, line.clone()).await;
                        append_log(&mut output_file, LogStream::Stderr, line).await?;
                    }
                    Ok(None) => {
                        stderr_done = true;
                    }
                    Err(e) => {
                        tracing::warn!("error reading stderr from child process: {}", e);
                        stderr_done = true;
                    }
                }
            }
        }
    }

    Ok(())
}

async fn append_log(file: &mut tokio::fs::File, stream: LogStream, content: String) -> Result<()> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())?;
    let line = format!("[{}][{:?}] {}\n", timestamp, stream, content);
    file.write_all(line.as_bytes()).await?;
    Ok(())
}

async fn push_log(buffer: &Arc<Mutex<VecDeque<LogLine>>>, stream: LogStream, content: String) {
    let mut buf = buffer.lock().await;
    if buf.len() >= LOG_BUFFER_LINES {
        buf.pop_front();
    }
    buf.push_back(LogLine {
        timestamp_secs: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0),
        stream,
        content,
    });
}

pub fn user_stop_marker_path(server: &str) -> Result<PathBuf> {
    let dir = data_dir()?.join("stopped");
    fs::create_dir_all(&dir).with_context(|| format!("failed to create '{}'", dir.display()))?;
    Ok(dir.join(server))
}

pub fn data_dir() -> Result<PathBuf> {
    let path = forge_home_dir()?;
    fs::create_dir_all(&path)
        .with_context(|| format!("failed to create forge data directory '{}'", path.display()))?;
    Ok(path)
}

pub fn run_pid_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("run.pid"))
}

pub fn write_run_pid() -> Result<()> {
    let path = run_pid_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, format!("{}", std::process::id()))
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

pub fn remove_run_pid() -> Result<()> {
    let path = run_pid_path()?;
    let _ = fs::remove_file(&path);
    Ok(())
}

pub fn state_file_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("state.json"))
}

pub fn logs_dir_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("logs"))
}

/// Returns the raw `FORGE_HOME` override if set to a non-blank value,
/// else `None`. Shared by `forge_home_dir` (which also falls back to
/// `$HOME/.forge`) and the keychain service name in `config::secret`, so
/// both stay in sync on what counts as "FORGE_HOME is active".
pub(crate) fn forge_home_override() -> Option<String> {
    let raw = env::var("FORGE_HOME").ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

fn forge_home_dir() -> Result<PathBuf> {
    // FORGE_HOME lets tests (and advanced users) redirect all forge data without
    // touching HOME, which is unsafe to mutate in a multithreaded process.
    if let Some(dir) = forge_home_override() {
        return Ok(PathBuf::from(dir));
    }
    let home = env::var("HOME").context("HOME environment variable is not set")?;
    Ok(PathBuf::from(home).join(".forge"))
}

fn write_state_file(
    state_path: &Path,
    started_at_secs: u64,
    handles: &HashMap<String, ServerHandle>,
) -> Result<()> {
    let mut servers = HashMap::new();
    for (name, handle) in handles.iter() {
        let health = handle.health_rx.borrow().clone();
        servers.insert(name.clone(), health.to_server_state());
    }

    let state = PersistentState {
        started_at_secs,
        servers,
    };

    let json = serde_json::to_string_pretty(&state)?;
    // Atomic write: write to a temp file then rename so readers never see a
    // partially-written state.json (matches the pattern used by config::save_to_file).
    let dir = state_path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(".forge_state_tmp_{}", std::process::id()));
    fs::write(&tmp, &json)
        .with_context(|| format!("failed to write temp state file '{}'", tmp.display()))?;
    fs::rename(&tmp, state_path).with_context(|| {
        format!(
            "failed to rename temp state file to '{}'",
            state_path.display()
        )
    })?;
    Ok(())
}

/// Write `state.json` after a successful proxy startup (stdio + HTTP/SSE backends).
pub fn write_proxy_startup_state(
    config: &ForgeConfig,
    registry: &crate::mcp::ToolRegistry,
) -> Result<()> {
    use crate::config::Transport;

    let started_at_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())?;

    let pids = registry.pids();
    let mut servers = HashMap::new();
    for (name, srv) in &config.server {
        let pid = pids.get(name).and_then(|entry| *entry.value());
        let (status, url, transport) = match srv.transport {
            Transport::Stdio => (
                if pid.is_some() {
                    "running".to_owned()
                } else {
                    "connected".to_owned()
                },
                None,
                Some(srv.transport.as_str().to_owned()),
            ),
            Transport::Http | Transport::Sse => (
                "connected".to_owned(),
                srv.url.clone(),
                Some(srv.transport.as_str().to_owned()),
            ),
        };
        servers.insert(
            name.clone(),
            ServerState {
                status,
                pid,
                uptime_secs: Some(0),
                restarts: 0,
                last_error: None,
                url,
                transport,
            },
        );
    }

    let state = PersistentState {
        started_at_secs,
        servers,
    };
    let path = state_file_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(&state)?;
    fs::write(&path, json)
        .with_context(|| format!("failed to write state file '{}'", path.display()))?;
    Ok(())
}

impl ServerHealth {
    fn to_server_state(&self) -> ServerState {
        match self {
            ServerHealth::Starting => ServerState {
                status: "starting".to_owned(),
                pid: None,
                uptime_secs: None,
                restarts: 0,
                last_error: None,
                url: None,
                transport: None,
            },
            ServerHealth::Running {
                pid,
                uptime_secs,
                restarts,
            } => ServerState {
                status: "running".to_owned(),
                pid: Some(*pid),
                uptime_secs: Some(*uptime_secs),
                restarts: *restarts,
                last_error: None,
                url: None,
                transport: None,
            },
            ServerHealth::Degraded {
                restarts,
                last_error,
            } => ServerState {
                status: "degraded".to_owned(),
                pid: None,
                uptime_secs: None,
                restarts: *restarts,
                last_error: Some(last_error.clone()),
                url: None,
                transport: None,
            },
            ServerHealth::Stopped => ServerState {
                status: "stopped".to_owned(),
                pid: None,
                uptime_secs: None,
                restarts: 0,
                last_error: None,
                url: None,
                transport: None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::forge_home_override;
    use std::sync::{Mutex, OnceLock};

    /// Serializes tests that mutate `FORGE_HOME`. Mirrors the convention in
    /// `crates/forge-core/tests/supervisor.rs` — a private lock per test
    /// binary, since `std::env::set_var` is `unsafe` in Rust >= 1.80 and
    /// this test binary runs tests in parallel threads by default.
    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn forge_home_override_returns_none_when_unset() {
        let _guard = env_lock().lock().unwrap();
        // SAFETY: serialized by env_lock(); FORGE_HOME is Forge-private
        // and not read by any system library.
        unsafe { std::env::remove_var("FORGE_HOME") };
        assert_eq!(forge_home_override(), None);
    }

    #[test]
    fn forge_home_override_returns_none_when_blank() {
        let _guard = env_lock().lock().unwrap();
        // SAFETY: serialized by env_lock().
        unsafe { std::env::set_var("FORGE_HOME", "   ") };
        assert_eq!(forge_home_override(), None);
        // SAFETY: serialized by env_lock().
        unsafe { std::env::remove_var("FORGE_HOME") };
    }

    #[test]
    fn forge_home_override_returns_trimmed_value_when_set() {
        let _guard = env_lock().lock().unwrap();
        // SAFETY: serialized by env_lock().
        unsafe { std::env::set_var("FORGE_HOME", "  /tmp/client-a  ") };
        assert_eq!(forge_home_override(), Some("/tmp/client-a".to_owned()));
        // SAFETY: serialized by env_lock().
        unsafe { std::env::remove_var("FORGE_HOME") };
    }
}
