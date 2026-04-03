use anyhow::{Context, Result};
use clap::Args;
use std::fs;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::thread;
use std::time::Duration;

use forge_core::supervisor::logs_dir_path;

#[derive(Debug, Args)]
#[command(about = "Show or follow logs for an MCP server")]
pub struct Logs {
    #[arg(help = "Server name")]
    pub server: String,

    #[arg(long, help = "Stream logs in real time")]
    pub follow: bool,

    #[arg(long, default_value_t = 50, help = "Number of lines to show")]
    pub lines: usize,
}

impl Logs {
    pub fn run(&self) -> Result<()> {
        let log_path = logs_dir_path()?.join(format!("{}.log", self.server));
        if !log_path.exists() {
            return Err(anyhow::anyhow!(
                "log file not found: {}",
                log_path.display()
            ));
        }

        let contents = fs::read_to_string(&log_path)
            .with_context(|| format!("failed to read log file {}", log_path.display()))?;
        let lines: Vec<_> = contents.lines().map(String::from).collect();
        let start = lines.len().saturating_sub(self.lines);
        for line in &lines[start..] {
            println!("{}", line);
        }

        if self.follow {
            let file = fs::File::open(&log_path)?;
            let mut reader = BufReader::new(file);
            reader.seek(SeekFrom::End(0))?;
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line)? {
                    0 => thread::sleep(Duration::from_millis(250)),
                    _ => print!("{}", line),
                }
            }
        }

        Ok(())
    }
}
