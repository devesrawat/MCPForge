use anyhow::{Context, Result};
use clap::Args;
use serde_json;
use std::fs;
use std::thread;
use std::time::Duration;

use forge_core::supervisor::{PersistentState, state_file_path};

#[derive(Debug, Args)]
pub struct Status {
    #[arg(long, help = "Update status every 2 seconds")]
    pub watch: bool,
}

impl Status {
    pub fn run(&self) -> Result<()> {
        if self.watch {
            loop {
                Self::print_status()?;
                thread::sleep(Duration::from_secs(2));
            }
        } else {
            Self::print_status()
        }
    }

    fn print_status() -> Result<()> {
        let path = state_file_path()?;
        let contents = fs::read_to_string(&path)
            .with_context(|| format!("failed to read state file {}", path.display()))?;
        let state: PersistentState = serde_json::from_str(&contents)
            .with_context(|| format!("failed to parse state file {}", path.display()))?;

        println!("started_at_secs: {}", state.started_at_secs);
        println!("servers:");
        for (name, info) in state.servers {
            println!(
                "- {}: {} pid={:?} uptime={:?} restarts={} last_error={:?}",
                name, info.status, info.pid, info.uptime_secs, info.restarts, info.last_error
            );
        }

        Ok(())
    }
}
