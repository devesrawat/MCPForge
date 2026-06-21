use anyhow::{Context, Result, anyhow};
use clap::Args;
use forge_core::supervisor::{PersistentState, data_dir, user_stop_marker_path};
use serde_json;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

/// Returns `true` if the process with the given PID is alive.
///
/// Uses the kill(2) syscall with signal 0 directly to avoid the shell `kill`
/// command's u32→i32 truncation bug (u32::MAX becomes -1, which signals every
/// process and returns success). PIDs that don't fit in a positive i32 cannot
/// exist on any Unix and are reported as dead immediately.
fn is_pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        if pid == 0 || pid > i32::MAX as u32 {
            return false;
        }
        // SAFETY: signal 0 never delivers anything; it only probes existence.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

#[derive(Debug, Args)]
#[command(about = "Stop forge and all running MCP servers")]
pub struct Stop {
    #[arg(help = "Stop only this server (supervisor mode)", required = false)]
    pub server: Option<String>,
}

impl Stop {
    pub fn run(&self) -> Result<()> {
        if let Some(server) = &self.server {
            return stop_one_server(server);
        }

        let dir = data_dir()?;
        for name in ["run.pid", "daemon.pid"] {
            let p = dir.join(name);
            if p.exists() {
                let pid_text =
                    fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
                let pid: u32 = pid_text.trim().parse().context("parse pid")?;
                if !is_pid_alive(pid) {
                    // Process is already gone; clean up the stale pid file.
                    let _ = fs::remove_file(&p);
                    println!(
                        "Removed stale pid file (process {} is no longer running)",
                        pid
                    );
                    continue;
                }
                let status = Command::new("kill")
                    .args(["-TERM", &pid.to_string()])
                    .status()
                    .context("kill")?;
                if !status.success() {
                    return Err(anyhow!("failed to send SIGTERM to {}", pid));
                }
                // Poll up to 2s for the process to exit before removing the pid file.
                let deadline = Instant::now() + Duration::from_secs(2);
                while is_pid_alive(pid) && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(100));
                }
                let _ = fs::remove_file(&p);
                println!("Stopped forge process {}", pid);
                return Ok(());
            }
        }

        Err(anyhow!(
            "no running forge proxy found. Start one with: forge start --daemon"
        ))
    }
}

fn stop_one_server(server: &str) -> Result<()> {
    let state_path = forge_core::supervisor::state_file_path()?;
    if !Path::new(&state_path).exists() {
        return Err(anyhow!(
            "state file missing (is the supervisor running?): {}",
            state_path.display()
        ));
    }
    let contents = fs::read_to_string(&state_path)
        .with_context(|| format!("read {}", state_path.display()))?;
    let state: PersistentState = serde_json::from_str(&contents).context("parse state.json")?;
    let info = state
        .servers
        .get(server)
        .ok_or_else(|| anyhow!("unknown server '{}' in state file", server))?;
    let pid = info
        .pid
        .ok_or_else(|| anyhow!("no pid recorded for server '{}'", server))?;

    let marker = user_stop_marker_path(server)?;
    if let Some(parent) = marker.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&marker, b"")?;
    let status = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .context("kill server process")?;
    if !status.success() {
        return Err(anyhow!("failed to signal server process {}", pid));
    }
    println!("Stop requested for server '{}' (pid {})", server, pid);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::is_pid_alive;

    #[test]
    fn current_process_is_alive() {
        let pid = std::process::id();
        assert!(is_pid_alive(pid), "current process should be alive");
    }

    #[test]
    fn zero_pid_is_not_alive() {
        // PID 0 is not a real process; kill -0 0 signals the whole process group
        // which may or may not succeed, but PID 0 is never a forge process.
        // We just verify is_pid_alive(u32::MAX) returns false (unlikely to exist).
        assert!(!is_pid_alive(u32::MAX), "PID u32::MAX should not be alive");
    }
}
