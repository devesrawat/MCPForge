use anyhow::{Context, Result};
use clap::Args;
use forge_core::config::ForgeConfig;
use std::path::PathBuf;

#[derive(Debug, Args)]
pub struct Remove {
    #[arg(help = "Server name")]
    pub name: String,
}

impl Remove {
    pub fn run(&self) -> Result<()> {
        let path = PathBuf::from("forge.toml");
        let mut cfg = ForgeConfig::load_from_file(&path)
            .with_context(|| format!("load {}", path.display()))?;
        if cfg.server.remove(&self.name).is_none() {
            anyhow::bail!("server '{}' not found", self.name);
        }
        cfg.save_to_file(&path)
            .with_context(|| format!("write {}", path.display()))?;
        println!("Removed server '{}' from forge.toml", self.name);
        Ok(())
    }
}
