use anyhow::{Context, Result};
use clap::Args;
use forge_core::config::{ForgeConfig, ServerConfig, Transport, validate_server_name};
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Args)]
#[command(about = "Add an MCP server to forge.toml")]
pub struct Add {
    #[arg(help = "Server name (TOML key)")]
    pub name: String,

    #[arg(long, help = "Command to launch the MCP server")]
    pub cmd: String,
}

impl Add {
    pub fn run(&self) -> Result<()> {
        self.run_in_dir(Path::new("."))
    }

    fn run_in_dir(&self, dir: &Path) -> Result<()> {
        let path = dir.join("forge.toml");
        let mut cfg = if path.exists() {
            ForgeConfig::load_from_file(&path)?
        } else {
            ForgeConfig {
                server: HashMap::new(),
                guard: Default::default(),
                proxy: Default::default(),
            }
        };

        validate_server_name(&self.name).map_err(|e| anyhow::anyhow!("{}", e))?;

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
                max_restarts: None,
            },
        );

        // Enable the HTTP proxy by default so clients can connect after forge start.
        if !cfg.proxy.enabled {
            cfg.proxy.enabled = true;
            println!("Proxy enabled — run `forge start` to activate.");
        }

        cfg.save_to_file(&path)
            .with_context(|| format!("write {}", path.display()))?;
        println!("Added server '{}' to {}", self.name, path.display());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn add(name: &str, cmd: &str) -> Add {
        Add {
            name: name.to_owned(),
            cmd: cmd.to_owned(),
        }
    }

    #[test]
    fn creates_config_when_file_missing() {
        let dir = TempDir::new().unwrap();
        add("srv", "echo hello").run_in_dir(dir.path()).unwrap();
        let cfg = ForgeConfig::load_from_file(dir.path().join("forge.toml")).unwrap();
        assert!(cfg.server.contains_key("srv"));
        assert_eq!(cfg.server["srv"].cmd, "echo hello");
    }

    #[test]
    fn adds_to_existing_config() {
        let dir = TempDir::new().unwrap();
        add("first", "cmd1").run_in_dir(dir.path()).unwrap();
        add("second", "cmd2").run_in_dir(dir.path()).unwrap();
        let cfg = ForgeConfig::load_from_file(dir.path().join("forge.toml")).unwrap();
        assert!(cfg.server.contains_key("first"));
        assert!(cfg.server.contains_key("second"));
    }

    #[test]
    fn rejects_duplicate_server() {
        let dir = TempDir::new().unwrap();
        add("dup", "cmd").run_in_dir(dir.path()).unwrap();
        let err = add("dup", "cmd2").run_in_dir(dir.path()).unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn rejects_invalid_server_name() {
        let dir = TempDir::new().unwrap();
        let err = add("bad name!", "cmd").run_in_dir(dir.path()).unwrap_err();
        assert!(err.to_string().to_lowercase().contains("invalid"));
    }

    #[test]
    fn enables_proxy_by_default() {
        let dir = TempDir::new().unwrap();
        add("srv", "cmd").run_in_dir(dir.path()).unwrap();
        let cfg = ForgeConfig::load_from_file(dir.path().join("forge.toml")).unwrap();
        assert!(cfg.proxy.enabled);
    }
}
