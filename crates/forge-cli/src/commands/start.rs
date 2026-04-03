use anyhow::{Context, Result};
use clap::Args;
use forge_core::audit::AuditWriter;
use forge_core::config::ForgeConfig;
use forge_core::mcp::build_tool_registry;
use forge_core::supervisor::{Supervisor, remove_run_pid, write_run_pid};
use forge_proxy::{ProxyAppState, build_router};
use std::fs;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::runtime::Builder;

#[derive(Debug, Args)]
#[command(about = "Start all configured MCP servers (and the proxy)")]
pub struct Start {
    #[arg(long, help = "Run the supervisor in daemon mode")]
    pub daemon: bool,

    #[arg(long, hide = true)]
    pub foreground: bool,

    #[arg(
        long,
        default_value = "forge.toml",
        help = "Path to the forge config file"
    )]
    pub config: PathBuf,
}

impl Start {
    pub fn run(&self) -> Result<()> {
        if self.daemon && !self.foreground {
            return self.spawn_daemon();
        }

        let config = ForgeConfig::load_from_file(&self.config)
            .with_context(|| format!("failed to load config from {}", self.config.display()))?;

        write_run_pid().context("failed to write ~/.forge/run.pid")?;

        let out = if config.proxy.enabled {
            self.run_proxy(config)
        } else {
            self.run_supervisor_only(config)
        };

        let _ = remove_run_pid();
        out
    }

    fn run_supervisor_only(&self, config: ForgeConfig) -> Result<()> {
        let mut supervisor = Supervisor::new(config)?;
        let runtime = Builder::new_current_thread().enable_all().build()?;
        runtime.block_on(supervisor.start_all())?;
        Ok(())
    }

    fn run_proxy(&self, config: ForgeConfig) -> Result<()> {
        let rt = Builder::new_multi_thread().enable_all().build()?;
        rt.block_on(async move {
            let registry = build_tool_registry(&config).await?;
            let audit_path = AuditWriter::default_path()?;
            let audit = Arc::new(AuditWriter::new(audit_path)?);
            let state = ProxyAppState::new(registry, config, Some(audit))?;
            let addr = format!("{}:{}", state.config.proxy.bind, state.config.proxy.port);
            let listener = tokio::net::TcpListener::bind(&addr)
                .await
                .with_context(|| format!("failed to bind {}", addr))?;
            tracing::info!(listen = %addr, "MCP proxy listening (POST / for JSON-RPC)");

            axum::serve(listener, build_router(state))
                .with_graceful_shutdown(async {
                    let _ = tokio::signal::ctrl_c().await;
                })
                .await
                .map_err(|e| anyhow::anyhow!("server exited with error: {}", e))?;

            Ok::<(), anyhow::Error>(())
        })?;

        Ok(())
    }

    fn spawn_daemon(&self) -> Result<()> {
        let exe = std::env::current_exe()?;
        let mut cmd = std::process::Command::new(exe);
        cmd.arg("start")
            .arg("--foreground")
            .arg("--config")
            .arg(self.config.to_string_lossy().as_ref())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let child = cmd.spawn()?;
        let pid = child.id();
        let pid_path = forge_core::supervisor::state_file_path()?;
        let pid_path = pid_path.with_file_name("daemon.pid");
        if let Some(parent) = pid_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&pid_path, pid.to_string())?;
        println!("Started daemon with PID {}", pid);
        Ok(())
    }
}
