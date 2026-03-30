use anyhow::{Context, Result};
use clap::Args;
use forge_core::config::{ForgeConfig, ServerConfig, Transport};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Args)]
pub struct Add {
    #[arg(help = "Server name (TOML key)")]
    pub name: String,

    #[arg(long, help = "Command to launch the MCP server")]
    pub cmd: String,
}

impl Add {
    pub fn run(&self) -> Result<()> {
        let path = PathBuf::from("forge.toml");
        let mut cfg = if path.exists() {
            ForgeConfig::load_from_file(&path)?
        } else {
            ForgeConfig {
                server: HashMap::new(),
                guard: Default::default(),
                proxy: Default::default(),
            }
        };

        if cfg.server.contains_key(&self.name) {
            anyhow::bail!("server '{}' already exists in forge.toml", self.name);
        }

        cfg.server.insert(
            self.name.clone(),
            ServerConfig {
                cmd: self.cmd.clone(),
                transport: Transport::Stdio,
                secret: HashMap::new(),
                allowed_tools: Vec::new(),
                deny_tools: Vec::new(),
                max_calls_per_min: 60,
                max_calls_per_day: None,
                tags: Vec::new(),
                env: HashMap::new(),
                ready_timeout_secs: None,
                estimated_cost_per_call_usd: None,
            },
        );

        cfg.save_to_file(&path)
            .with_context(|| format!("write {}", path.display()))?;
        println!("Added server '{}' to {}", self.name, path.display());
        Ok(())
    }
}
