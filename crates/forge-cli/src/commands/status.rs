use anyhow::{Context, Result};
use clap::Args;
use serde_json;
use std::fs;
use std::thread;
use std::time::Duration;

use forge_core::supervisor::{PersistentState, state_file_path};

#[derive(Debug, Args)]
#[command(about = "Show running status of all MCP servers")]
pub struct Status {
    #[arg(long, help = "Update status every 2 seconds")]
    pub watch: bool,

    #[arg(long, help = "Emit JSON instead of table output")]
    pub json: bool,
}

impl Status {
    pub fn run(&self) -> Result<()> {
        if self.watch {
            loop {
                self.print_status()?;
                thread::sleep(Duration::from_secs(2));
            }
        } else {
            self.print_status()
        }
    }

    fn print_status(&self) -> Result<()> {
        let path = state_file_path()?;
        let contents = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if self.json {
                    println!("{}", serde_json::json!({ "status": "not running" }));
                } else {
                    println!("forge is not running (no state file found)");
                }
                return Ok(());
            }
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("failed to read state file {}", path.display()));
            }
        };
        let state: PersistentState = serde_json::from_str(&contents)
            .with_context(|| format!("failed to parse state file {}", path.display()))?;

        if self.json {
            println!("{}", serde_json::to_string_pretty(&state)?);
            return Ok(());
        }

        println!("STARTED_AT_SECS {}", state.started_at_secs);
        println!(
            "{:<18} {:<10} {:<8} {:<10} {:<9} LAST_ERROR",
            "NAME", "STATUS", "PID", "UPTIME", "RESTARTS"
        );
        for (name, info) in state.servers {
            let pid = info
                .pid
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".to_owned());
            let uptime = info
                .uptime_secs
                .map(|v| format!("{}s", v))
                .unwrap_or_else(|| "-".to_owned());
            let last_error = info.last_error.unwrap_or_else(|| "-".to_owned());
            println!(
                "{:<18} {:<10} {:<8} {:<10} {:<9} {}",
                name, info.status, pid, uptime, info.restarts, last_error
            );
        }

        Ok(())
    }
}
