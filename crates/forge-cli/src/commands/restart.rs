use anyhow::{Context, Result, anyhow};
use clap::Args;
use forge_core::supervisor::{PersistentState, state_file_path, user_stop_marker_path};
use serde_json;
use std::fs;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Args)]
#[command(about = "Restart a specific MCP server")]
pub struct Restart {
    #[arg(help = "Server name to restart")]
    pub server: String,
}

impl Restart {
    pub fn run(&self) -> Result<()> {
        let marker = user_stop_marker_path(&self.server)?;
        let _ = fs::remove_file(&marker);

        let state_path = state_file_path()?;
        if !Path::new(&state_path).exists() {
            return Err(anyhow!("state file missing: {}", state_path.display()));
        }
        let contents = fs::read_to_string(&state_path)
            .with_context(|| format!("read {}", state_path.display()))?;
        let state: PersistentState = serde_json::from_str(&contents).context("parse state")?;
        let info = state
            .servers
            .get(&self.server)
            .ok_or_else(|| anyhow!("unknown server '{}'", self.server))?;
        let pid = info
            .pid
            .ok_or_else(|| anyhow!("no pid for server '{}'", self.server))?;

        let status = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()
            .context("kill")?;
        if !status.success() {
            return Err(anyhow!("failed to signal process {}", pid));
        }
        println!(
            "Restart requested for '{}' — supervisor will respawn after pid {} exits",
            self.server, pid
        );
        Ok(())
    }
}
