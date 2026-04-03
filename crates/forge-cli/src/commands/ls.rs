use anyhow::{Context, Result};
use clap::Args;
use forge_core::config::ForgeConfig;
use std::path::PathBuf;

#[derive(Debug, Args)]
#[command(about = "List configured MCP servers")]
pub struct Ls {}

impl Ls {
    pub fn run(&self) -> Result<()> {
        let path = PathBuf::from("forge.toml");
        if !path.exists() {
            println!("no forge.toml in current directory");
            return Ok(());
        }
        let cfg = ForgeConfig::load_from_file(&path)
            .with_context(|| format!("load {}", path.display()))?;
        if cfg.server.is_empty() {
            println!("(no servers configured)");
            return Ok(());
        }
        let mut names: Vec<_> = cfg.server.keys().cloned().collect();
        names.sort();
        for name in names {
            let s = cfg.server.get(&name).expect("key");
            println!("{}\t{}", name, s.cmd);
        }
        Ok(())
    }
}
