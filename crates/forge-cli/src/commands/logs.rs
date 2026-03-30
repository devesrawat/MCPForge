use anyhow::{Context, Result};
use clap::Args;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::thread;
use std::time::Duration;

use forge_core::supervisor::logs_dir_path;

#[derive(Debug, Args)]
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
            let mut file = fs::File::open(&log_path)?;
            file.seek(SeekFrom::End(0))?;
            loop {
                let mut buffer = String::new();
                let bytes_read = file.read_to_string(&mut buffer)?;
                if bytes_read > 0 {
                    print!("{}", buffer);
                }
                thread::sleep(Duration::from_secs(1));
            }
        }

        Ok(())
    }
}
