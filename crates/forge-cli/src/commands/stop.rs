use anyhow::{Context, Result, anyhow};
use clap::Args;
use forge_core::supervisor::{PersistentState, data_dir, user_stop_marker_path};
use serde_json;
use std::fs;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Args)]
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
                let status = Command::new("kill")
                    .args(["-TERM", &pid.to_string()])
                    .status()
                    .context("kill")?;
                if !status.success() {
                    return Err(anyhow!("failed to send SIGTERM to {}", pid));
                }
                let _ = fs::remove_file(&p);
                println!("Stopped forge process {}", pid);
                return Ok(());
            }
        }

        Err(anyhow!(
            "no ~/.forge/run.pid or daemon.pid found; forge does not appear to be running"
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
