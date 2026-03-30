use anyhow::{Context, Result};
use chrono::{DateTime, Duration, TimeZone, Utc};
use clap::{Args, ValueEnum};
use forge_core::audit::{AuditQuery, AuditReader};

#[derive(Debug, Clone, ValueEnum)]
pub enum OutputFormat {
    Text,
    Json,
}

#[derive(Debug, Args)]
pub struct Audit {
    #[arg(long, help = "Filter by server name")]
    pub server: Option<String>,

    #[arg(long, help = "Filter by tool name")]
    pub tool: Option<String>,

    #[arg(long, help = "Time window, e.g. 24h, 7d")]
    pub since: Option<String>,

    #[arg(long, help = "Only show failures")]
    pub errors: bool,

    #[arg(long, help = "Print aggregate stats only")]
    pub stats: bool,

    #[arg(long, default_value_t = OutputFormat::Text, value_enum)]
    pub format: OutputFormat,

    #[arg(long, default_value_t = 50, help = "Maximum number of events to show")]
    pub limit: usize,
}

impl Audit {
    pub fn run(&self) -> Result<()> {
        let reader = AuditReader::open_default().context("failed to open audit database")?;
        let since = self.since.as_deref().map(parse_since).transpose()?;

        let query = AuditQuery {
            server: self.server.clone(),
            tool: self.tool.clone(),
            since,
            errors_only: self.errors,
        };

        let rows = reader
            .query_events(
                query.clone(),
                if self.stats { None } else { Some(self.limit) },
            )
            .context("failed to query audit events")?;

        if self.stats {
            let calls = rows.len();
            let errors = rows.iter().filter(|r| r.result_code != 0).count();
            let avg_lat = if calls == 0 {
                0.0
            } else {
                rows.iter().map(|r| r.latency_ms as f64).sum::<f64>() / calls as f64
            };
            match self.format {
                OutputFormat::Json => {
                    println!(
                        "{}",
                        serde_json::json!({ "calls": calls, "errors": errors, "avg_latency_ms": avg_lat })
                    );
                }
                OutputFormat::Text => {
                    println!(
                        "audit stats: calls={} errors={} avg_latency_ms={:.1}",
                        calls, errors, avg_lat
                    );
                }
            }
            return Ok(());
        }

        match self.format {
            OutputFormat::Json => {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            }
            OutputFormat::Text => {
                if rows.is_empty() {
                    println!("no audit events found");
                }
                for record in rows {
                    println!(
                        "{} | {} | {} | rc={} | lat={}ms | error={:?} | session={:?}",
                        format_timestamp(record.ts),
                        record.server,
                        record.tool,
                        record.result_code,
                        record.latency_ms,
                        record.error,
                        record.session_id,
                    );
                }
            }
        }

        Ok(())
    }
}

fn parse_since(text: &str) -> Result<DateTime<Utc>> {
    let now = Utc::now();
    if let Some(value) = text.strip_suffix('s') {
        let seconds: i64 = value.parse()?;
        Ok(now - Duration::seconds(seconds))
    } else if let Some(value) = text.strip_suffix('m') {
        let minutes: i64 = value.parse()?;
        Ok(now - Duration::minutes(minutes))
    } else if let Some(value) = text.strip_suffix('h') {
        let hours: i64 = value.parse()?;
        Ok(now - Duration::hours(hours))
    } else if let Some(value) = text.strip_suffix('d') {
        let days: i64 = value.parse()?;
        Ok(now - Duration::days(days))
    } else {
        Err(anyhow::anyhow!("unsupported since format: {}", text))
    }
}

fn format_timestamp(ms: i64) -> String {
    Utc.timestamp_millis_opt(ms)
        .single()
        .unwrap_or_else(|| Utc.timestamp_opt(0, 0).single().unwrap())
        .to_rfc3339()
}
