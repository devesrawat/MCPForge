use anyhow::{Context, Result};
use clap::Args;
use forge_core::config::{
    ForgeConfig, ServerConfig, Transport, validate_server_name, validate_server_transport,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Args)]
#[command(about = "Add an MCP server to forge.toml")]
pub struct Add {
    #[arg(help = "Server name (TOML key)")]
    pub name: String,

    #[arg(
        long,
        default_value = "stdio",
        help = "Transport type: stdio, http, or sse"
    )]
    pub transport: String,

    #[arg(
        long,
        help = "Command to launch the MCP server (required for stdio transport)"
    )]
    pub cmd: Option<String>,

    #[arg(long, help = "Base URL for http or sse transport")]
    pub url: Option<String>,

    #[arg(
        long,
        default_value = "forge.toml",
        help = "Path to the forge config file"
    )]
    pub config: PathBuf,
}

impl Add {
    pub fn run(&self) -> Result<()> {
        self.run_at_config(&self.config)
    }

    fn run_at_config(&self, path: &Path) -> Result<()> {
        let mut cfg = if path.exists() {
            ForgeConfig::load_from_file(path)?
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

        let transport = parse_transport(&self.transport)?;
        let server = ServerConfig {
            cmd: self.cmd.clone(),
            transport,
            url: self.url.clone(),
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
        };

        validate_server_transport(&self.name, &server).map_err(|e| anyhow::anyhow!("{}", e.0))?;

        cfg.server.insert(self.name.clone(), server);

        // Enable the HTTP proxy by default so clients can connect after forge start.
        if !cfg.proxy.enabled {
            cfg.proxy.enabled = true;
            println!("Proxy enabled — run `forge start` to activate.");
        }

        cfg.save_to_file(path)
            .with_context(|| format!("write {}", path.display()))?;
        println!("Added server '{}' to {}", self.name, path.display());
        Ok(())
    }
}

fn parse_transport(s: &str) -> Result<Transport> {
    match s.to_ascii_lowercase().as_str() {
        "stdio" => Ok(Transport::Stdio),
        "http" => Ok(Transport::Http),
        "sse" => Ok(Transport::Sse),
        other => anyhow::bail!(
            "invalid transport '{}'; expected one of: stdio, http, sse",
            other
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn add(name: &str, cmd: Option<&str>, transport: &str, url: Option<&str>) -> Add {
        Add {
            name: name.to_owned(),
            transport: transport.to_owned(),
            cmd: cmd.map(str::to_owned),
            url: url.map(str::to_owned),
            config: PathBuf::from("forge.toml"),
        }
    }

    fn run_add_in(dir: &Path, cmd: &Add) -> Result<()> {
        cmd.run_at_config(&dir.join("forge.toml"))
    }

    #[test]
    fn config_flag_uses_specified_path() {
        let dir = TempDir::new().unwrap();
        let custom_path = dir.path().join("custom.toml");
        let cmd = Add {
            name: "srv".to_owned(),
            transport: "stdio".to_owned(),
            cmd: Some("echo hello".to_owned()),
            url: None,
            config: custom_path.clone(),
        };
        cmd.run_at_config(&custom_path).unwrap();
        assert!(custom_path.exists(), "config written to custom path");
        let cfg = ForgeConfig::load_from_file(&custom_path).unwrap();
        assert!(cfg.server.contains_key("srv"));
    }

    #[test]
    fn creates_config_when_file_missing() {
        let dir = TempDir::new().unwrap();
        run_add_in(dir.path(), &add("srv", Some("echo hello"), "stdio", None)).unwrap();
        let cfg = ForgeConfig::load_from_file(dir.path().join("forge.toml")).unwrap();
        assert!(cfg.server.contains_key("srv"));
        assert_eq!(cfg.server["srv"].cmd.as_deref(), Some("echo hello"));
    }

    #[test]
    fn adds_http_server_with_url() {
        let dir = TempDir::new().unwrap();
        run_add_in(
            dir.path(),
            &add("remote", None, "http", Some("http://127.0.0.1:8080/mcp")),
        )
        .unwrap();
        let cfg = ForgeConfig::load_from_file(dir.path().join("forge.toml")).unwrap();
        assert_eq!(cfg.server["remote"].transport, Transport::Http);
        assert_eq!(
            cfg.server["remote"].url.as_deref(),
            Some("http://127.0.0.1:8080/mcp")
        );
    }

    #[test]
    fn adds_to_existing_config() {
        let dir = TempDir::new().unwrap();
        run_add_in(dir.path(), &add("first", Some("cmd1"), "stdio", None)).unwrap();
        run_add_in(dir.path(), &add("second", Some("cmd2"), "stdio", None)).unwrap();
        let cfg = ForgeConfig::load_from_file(dir.path().join("forge.toml")).unwrap();
        assert!(cfg.server.contains_key("first"));
        assert!(cfg.server.contains_key("second"));
    }

    #[test]
    fn rejects_duplicate_server() {
        let dir = TempDir::new().unwrap();
        run_add_in(dir.path(), &add("dup", Some("cmd"), "stdio", None)).unwrap();
        let err = run_add_in(dir.path(), &add("dup", Some("cmd2"), "stdio", None)).unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn rejects_invalid_server_name() {
        let dir = TempDir::new().unwrap();
        let err =
            run_add_in(dir.path(), &add("bad name!", Some("cmd"), "stdio", None)).unwrap_err();
        assert!(err.to_string().to_lowercase().contains("invalid"));
    }

    #[test]
    fn rejects_empty_cmd_for_stdio() {
        let dir = TempDir::new().unwrap();
        let err = run_add_in(dir.path(), &add("empty", Some(""), "stdio", None)).unwrap_err();
        assert!(err.to_string().contains("requires cmd"));
    }

    #[test]
    fn rejects_http_without_url() {
        let dir = TempDir::new().unwrap();
        let err = run_add_in(dir.path(), &add("remote", None, "http", None)).unwrap_err();
        assert!(err.to_string().contains("requires url"));
    }

    #[test]
    fn rejects_missing_cmd_for_stdio() {
        let dir = TempDir::new().unwrap();
        let err = run_add_in(dir.path(), &add("nocmd", None, "stdio", None)).unwrap_err();
        assert!(err.to_string().contains("requires cmd"));
    }

    #[test]
    fn rejects_invalid_transport() {
        let dir = TempDir::new().unwrap();
        let err = run_add_in(dir.path(), &add("bad", Some("cmd"), "websocket", None)).unwrap_err();
        assert!(err.to_string().contains("invalid transport"));
    }

    #[test]
    fn adds_sse_server_with_url() {
        let dir = TempDir::new().unwrap();
        run_add_in(
            dir.path(),
            &add("linear", None, "sse", Some("https://mcp.linear.app/sse")),
        )
        .unwrap();
        let cfg = ForgeConfig::load_from_file(dir.path().join("forge.toml")).unwrap();
        assert_eq!(cfg.server["linear"].transport, Transport::Sse);
        assert_eq!(
            cfg.server["linear"].url.as_deref(),
            Some("https://mcp.linear.app/sse")
        );
    }

    #[test]
    fn enables_proxy_by_default() {
        let dir = TempDir::new().unwrap();
        run_add_in(dir.path(), &add("srv", Some("cmd"), "stdio", None)).unwrap();
        let cfg = ForgeConfig::load_from_file(dir.path().join("forge.toml")).unwrap();
        assert!(cfg.proxy.enabled);
    }

    #[test]
    fn writes_minimal_toml_for_stdio_server() {
        let dir = TempDir::new().unwrap();
        run_add_in(dir.path(), &add("srv", Some("echo hello"), "stdio", None)).unwrap();
        let text = std::fs::read_to_string(dir.path().join("forge.toml")).unwrap();
        assert!(text.contains("[server.srv]"));
        assert!(text.contains("cmd = \"echo hello\""));
        assert!(!text.contains("allowed_tools"));
        assert!(!text.contains("transport = \"stdio\""));
        assert!(!text.contains("[server.srv.secret]"));
    }
}
