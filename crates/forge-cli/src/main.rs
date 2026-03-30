use anyhow::Result;
use clap::{Parser, Subcommand};

mod commands;
mod telemetry;
use commands::{Add, Audit, Init, Logs, Ls, Remove, Report, Restart, Secret, Start, Status, Stop};

#[derive(Debug, Parser)]
#[command(name = "forge")]
#[command(about = "mcp-forge CLI", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Add(Add),
    Audit(Audit),
    Init(Init),
    Ls(Ls),
    Remove(Remove),
    Report(Report),
    Secret(Secret),
    Start(Start),
    Stop(Stop),
    Restart(Restart),
    Status(Status),
    Logs(Logs),
}

fn main() -> Result<()> {
    #[cfg(feature = "otlp")]
    telemetry::init_otlp()?;

    #[cfg(not(feature = "otlp"))]
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .try_init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Add(cmd) => cmd.run(),
        Commands::Audit(cmd) => cmd.run(),
        Commands::Init(cmd) => cmd.run(),
        Commands::Ls(cmd) => cmd.run(),
        Commands::Remove(cmd) => cmd.run(),
        Commands::Report(cmd) => cmd.run(),
        Commands::Secret(cmd) => cmd.run(),
        Commands::Start(cmd) => cmd.run(),
        Commands::Stop(cmd) => cmd.run(),
        Commands::Restart(cmd) => cmd.run(),
        Commands::Status(cmd) => cmd.run(),
        Commands::Logs(cmd) => cmd.run(),
    }
}
