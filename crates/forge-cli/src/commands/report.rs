use anyhow::{Context, Result};
use chrono::{Duration, TimeZone, Utc};
use clap::{Args, ValueEnum};
use forge_core::audit::{AuditQuery, AuditReader};
use forge_core::config::ForgeConfig;
use serde::Serialize;
use serde_json::json;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

#[derive(Debug, Clone, ValueEnum)]
pub enum Period {
    Week,
    Month,
}

#[derive(Debug, Clone, ValueEnum)]
pub enum OutputFormat {
    Text,
    Json,
}

#[derive(Debug, Args)]
pub struct Report {
    #[arg(long, default_value_t = Period::Week, value_enum)]
    pub period: Period,

    #[arg(long, help = "Server name to filter")]
    pub server: Option<String>,

    #[arg(long, default_value_t = OutputFormat::Text, value_enum)]
    pub format: OutputFormat,
}

impl Report {
    pub fn run(&self) -> Result<()> {
        let reader = AuditReader::open_default().context("failed to open audit database")?;
        let since = Utc::now()
            - match self.period {
                Period::Week => Duration::days(7),
                Period::Month => Duration::days(30),
            };

        let query = AuditQuery {
            server: self.server.clone(),
            tool: None,
            since: Some(since),
            errors_only: false,
        };

        let events = reader
            .query_events(query, None)
            .context("failed to query audit events")?;

        let cost_per_server = load_cost_map();

        let rows = summarize_events(&events, &cost_per_server);
        let total = summarize_totals(&rows);

        match self.format {
            OutputFormat::Json => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "period": format!("{:?}", self.period),
                        "server": self.server,
                        "summary": rows,
                        "total": total,
                    }))?
                );
            }
            OutputFormat::Text => {
                println!("forge report ({:?})", self.period);
                println!("Server    Calls   Errors   Err%    Avg lat   P99 lat   Est cost");
                for row in &rows {
                    println!(
                        "{:<8} {:>6} {:>7} {:>6.1}% {:>8}ms {:>8}ms ${:>6.2}",
                        row.server,
                        row.calls,
                        row.errors,
                        row.error_rate * 100.0,
                        row.avg_latency,
                        row.p99_latency,
                        row.cost,
                    );
                }
                println!(
                    "{:<8} {:>6} {:>7} {:>6.1}% {:>8}ms {:>8}ms ${:>6.2}",
                    "TOTAL",
                    total.calls,
                    total.errors,
                    total.error_rate * 100.0,
                    total.avg_latency,
                    total.p99_latency,
                    total.cost,
                );

                if !events.is_empty() {
                    println!("\nDaily volume:");
                    let daily = daily_counts(&events);
                    for (day, count) in daily {
                        println!("{} | {:<20} | {}", day, bar(count, 20), count);
                    }
                }
            }
        }

        Ok(())
    }
}

#[derive(Debug, Serialize)]
struct ReportRow {
    server: String,
    calls: usize,
    errors: usize,
    error_rate: f64,
    avg_latency: f64,
    p99_latency: u64,
    cost: f64,
}

fn load_cost_map() -> HashMap<String, f64> {
    let path = PathBuf::from("forge.toml");
    let Ok(cfg) = ForgeConfig::load_from_file(&path) else {
        return HashMap::new();
    };
    cfg.server
        .into_iter()
        .filter_map(|(name, s)| s.estimated_cost_per_call_usd.map(|c| (name, c.max(0.0))))
        .collect()
}

fn summarize_events(
    events: &[forge_core::audit::AuditRecord],
    cost_map: &HashMap<String, f64>,
) -> Vec<ReportRow> {
    let mut grouped: HashMap<String, Vec<&forge_core::audit::AuditRecord>> = HashMap::new();
    for event in events {
        grouped.entry(event.server.clone()).or_default().push(event);
    }

    let mut rows: Vec<ReportRow> = grouped
        .into_iter()
        .map(|(server, events)| {
            let calls = events.len();
            let errors = events.iter().filter(|e| e.result_code != 0).count();
            let avg_latency = if calls == 0 {
                0.0
            } else {
                events.iter().map(|e| e.latency_ms as f64).sum::<f64>() / calls as f64
            };
            let mut latencies: Vec<u64> = events.iter().map(|e| e.latency_ms).collect();
            latencies.sort_unstable();
            let p99_latency = if latencies.is_empty() {
                0
            } else {
                let index = ((latencies.len() as f64) * 0.99).ceil() as usize;
                let index = index.saturating_sub(1).min(latencies.len() - 1);
                latencies[index]
            };
            let unit = cost_map.get(&server).copied().unwrap_or(0.0);
            let cost = (calls as f64) * unit;

            ReportRow {
                server,
                calls,
                errors,
                error_rate: if calls == 0 {
                    0.0
                } else {
                    errors as f64 / calls as f64
                },
                avg_latency,
                p99_latency,
                cost,
            }
        })
        .collect();

    rows.sort_by(|a, b| b.calls.cmp(&a.calls));
    rows
}

fn summarize_totals(rows: &[ReportRow]) -> ReportRow {
    let calls = rows.iter().map(|row| row.calls).sum();
    let errors = rows.iter().map(|row| row.errors).sum();
    let total_latency: f64 = rows
        .iter()
        .map(|row| row.avg_latency * row.calls as f64)
        .sum();
    let avg_latency = if calls == 0 {
        0.0
    } else {
        total_latency / calls as f64
    };
    let p99_latency = rows
        .iter()
        .flat_map(|row| std::iter::repeat_n(row.p99_latency, row.calls))
        .max()
        .unwrap_or(0);
    let cost: f64 = rows.iter().map(|row| row.cost).sum();

    ReportRow {
        server: "TOTAL".to_string(),
        calls,
        errors,
        error_rate: if calls == 0 {
            0.0
        } else {
            errors as f64 / calls as f64
        },
        avg_latency,
        p99_latency,
        cost,
    }
}

fn daily_counts(events: &[forge_core::audit::AuditRecord]) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for event in events {
        let day = Utc
            .timestamp_millis_opt(event.ts)
            .single()
            .unwrap_or_else(|| Utc.timestamp_opt(0, 0).single().unwrap())
            .date_naive()
            .format("%Y-%m-%d")
            .to_string();
        *counts.entry(day).or_default() += 1;
    }
    counts
}

fn bar(value: usize, width: usize) -> String {
    let normalized = std::cmp::min(value, width);
    "=".repeat(normalized)
}
