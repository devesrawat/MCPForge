use anyhow::{Context, Result};
use clap::Args;
use dialoguer::{Confirm, Input};
use forge_core::config::{ForgeConfig, SecretRef, ServerConfig, Transport, validate_server_name};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Args)]
#[command(about = "Interactively create a new forge.toml")]
pub struct Init {}

impl Init {
    pub fn run(&self) -> Result<()> {
        let path = PathBuf::from("forge.toml");
        if path.exists() {
            anyhow::bail!(
                "{} already exists; remove it or use forge add",
                path.display()
            );
        }

        let mut cfg = ForgeConfig {
            server: HashMap::new(),
            guard: Default::default(),
            proxy: Default::default(),
        };

        println!("mcp-forge init — add MCP servers (empty name to finish)");
        loop {
            let name: String = Input::new()
                .with_prompt("Server name")
                .allow_empty(true)
                .interact_text()?;
            if name.trim().is_empty() {
                break;
            }
            if let Err(e) = validate_server_name(name.trim()) {
                println!("  Invalid server name: {}", e);
                continue;
            }
            let cmd: String = Input::new().with_prompt("Launch command").interact_text()?;

            let mut secret_map = HashMap::new();
            let secret_line: String = Input::new()
                .with_prompt("Optional secret: env:VAR or keychain:name (blank to skip)")
                .allow_empty(true)
                .interact_text()?;
            if !secret_line.trim().is_empty() {
                let key_name: String = Input::new()
                    .with_prompt("Environment variable name to set with that secret")
                    .default("TOKEN".to_string())
                    .interact_text()?;
                secret_map.insert(key_name, parse_secret_ref_line(secret_line.trim())?);
            }

            cfg.server.insert(
                name.trim().to_owned(),
                ServerConfig {
                    cmd,
                    transport: Transport::Stdio,
                    secret: secret_map,
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
            println!("Added server '{}'", name.trim());

            if !Confirm::new()
                .with_prompt("Add another server?")
                .interact()?
            {
                break;
            }
        }

        if !cfg.server.is_empty()
            && Confirm::new()
                .with_prompt("Enable HTTP proxy on 127.0.0.1:3456?")
                .default(true)
                .interact()?
        {
            cfg.proxy.enabled = true;
        }

        cfg.save_to_file(&path)
            .with_context(|| format!("write {}", path.display()))?;
        println!("Wrote {}", path.display());
        Ok(())
    }
}

fn parse_secret_ref_line(s: &str) -> Result<SecretRef> {
    #[derive(serde::Deserialize)]
    struct W {
        v: SecretRef,
    }
    let t = format!("v = {:?}", s);
    let w: W = toml::from_str(&t).context("parse secret ref")?;
    Ok(w.v)
}
