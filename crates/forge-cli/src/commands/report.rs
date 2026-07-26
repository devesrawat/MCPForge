use anyhow::{Context, Result};
use chrono::{Duration, TimeZone, Utc};
use clap::{Args, ValueEnum};
use forge_core::audit::{AuditQuery, AuditReader, latency_ms_f64};
use forge_core::config::ForgeConfig;
use serde::Serialize;
use serde_json::json;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, ValueEnum)]
pub enum Period {
    Week,
    Month,
}

#[derive(Debug, Clone, ValueEnum)]
pub enum OutputFormat {
    Text,
    Json,
    Markdown,
}

#[derive(Debug, Args)]
#[command(about = "Print a usage and cost summary report")]
pub struct Report {
    #[arg(long, default_value_t = Period::Week, value_enum)]
    pub period: Period,

    #[arg(long, help = "Server name to filter")]
    pub server: Option<String>,

    #[arg(long, default_value_t = OutputFormat::Text, value_enum)]
    pub format: OutputFormat,

    #[arg(
        long,
        default_value = "forge.toml",
        help = "Path to the forge config file (used to look up estimated per-call costs)"
    )]
    pub config: PathBuf,
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

        let cost_per_server = load_cost_map(&self.config);

        let executed_events = filter_executed_events(&events);
        let rows = summarize_events(&executed_events, &cost_per_server);
        let total = summarize_totals(&rows, &executed_events);

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
            OutputFormat::Markdown => {
                print!(
                    "{}",
                    render_markdown(&self.period, &self.server, &rows, &total, &events)
                );
            }
            OutputFormat::Text => {
                println!("forge report ({:?})", self.period);
                println!("Server    Calls   Errors   Err%    Avg lat   P99 lat   Est cost");
                for row in &rows {
                    println!(
                        "{:<8} {:>6} {:>7} {:>6.1}% {:>8.2}ms {:>8.2}ms ${:>6.2}",
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
                    "{:<8} {:>6} {:>7} {:>6.1}% {:>8.2}ms {:>8.2}ms ${:>6.2}",
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
    p99_latency: f64,
    cost: f64,
}

fn load_cost_map(config_path: &Path) -> HashMap<String, f64> {
    let Ok(cfg) = ForgeConfig::load_from_file(config_path) else {
        return HashMap::new();
    };
    cfg.server
        .into_iter()
        .filter_map(|(name, s)| s.estimated_cost_per_call_usd.map(|c| (name, c.max(0.0))))
        .collect()
}

fn denial_reason(result_code: i32) -> Option<&'static str> {
    match result_code {
        forge_core::audit::RESULT_CODE_POLICY_DENIED => Some("policy-denied"),
        forge_core::audit::RESULT_CODE_RATE_LIMITED => Some("rate-limited"),
        forge_core::audit::RESULT_CODE_COST_LIMITED => Some("cost-limited"),
        forge_core::audit::RESULT_CODE_INJECTION_BLOCKED => Some("injection-blocked"),
        _ => None,
    }
}

/// Calls that never reached the upstream tool (denied, rate-limited,
/// cost-limited, or injection-blocked) always carry latency 0 and never
/// incurred real cost. Excluding them from the calls/cost/latency summary
/// prevents "Est cost" (calls * unit) from being inflated and avg/p99
/// latency from being pulled toward zero. They remain fully represented
/// via `denial_counts` on the unfiltered event set.
fn filter_executed_events(
    events: &[forge_core::audit::AuditRecord],
) -> Vec<forge_core::audit::AuditRecord> {
    events
        .iter()
        .filter(|e| denial_reason(e.result_code).is_none())
        .cloned()
        .collect()
}

fn denial_counts(
    events: &[forge_core::audit::AuditRecord],
) -> BTreeMap<(String, &'static str), usize> {
    let mut counts: BTreeMap<(String, &'static str), usize> = BTreeMap::new();
    for event in events {
        if let Some(reason) = denial_reason(event.result_code) {
            *counts.entry((event.server.clone(), reason)).or_default() += 1;
        }
    }
    counts
}

fn render_markdown(
    period: &Period,
    server_filter: &Option<String>,
    rows: &[ReportRow],
    total: &ReportRow,
    events: &[forge_core::audit::AuditRecord],
) -> String {
    let mut out = String::new();
    out.push_str(&format!("# Forge Report ({:?})\n\n", period));
    if let Some(s) = server_filter {
        out.push_str(&format!("Filtered to server: `{}`\n\n", s));
    }

    out.push_str("## Usage Summary\n\n");
    out.push_str("| Server | Calls | Errors | Err% | Avg lat | P99 lat | Est cost |\n");
    out.push_str("|---|---:|---:|---:|---:|---:|---:|\n");
    for row in rows {
        out.push_str(&format!(
            "| {} | {} | {} | {:.1}% | {:.2}ms | {:.2}ms | ${:.2} |\n",
            row.server,
            row.calls,
            row.errors,
            row.error_rate * 100.0,
            row.avg_latency,
            row.p99_latency,
            row.cost,
        ));
    }
    out.push_str(&format!(
        "| **TOTAL** | {} | {} | {:.1}% | {:.2}ms | {:.2}ms | ${:.2} |\n\n",
        total.calls,
        total.errors,
        total.error_rate * 100.0,
        total.avg_latency,
        total.p99_latency,
        total.cost,
    ));

    out.push_str("## Denials by Reason\n\n");
    let denials = denial_counts(events);
    if denials.is_empty() {
        out.push_str(
            "No denied, rate-limited, cost-limited, or injection-blocked calls in this period.\n",
        );
    } else {
        out.push_str("| Server | Reason | Count |\n");
        out.push_str("|---|---|---:|\n");
        for ((server, reason), count) in &denials {
            out.push_str(&format!("| {} | {} | {} |\n", server, reason, count));
        }
    }

    out
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
                events.iter().map(|&e| latency_ms_f64(e)).sum::<f64>() / calls as f64
            };
            let mut latencies: Vec<f64> = events.iter().map(|&e| latency_ms_f64(e)).collect();
            latencies.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let p99_latency = if latencies.is_empty() {
                0.0
            } else {
                let index = latencies.len().saturating_mul(99).div_ceil(100);
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

    rows.sort_by_key(|row| std::cmp::Reverse(row.calls));
    rows
}

fn summarize_totals(rows: &[ReportRow], events: &[forge_core::audit::AuditRecord]) -> ReportRow {
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
    // Compute true combined p99 from raw event latencies.
    let mut latencies: Vec<f64> = events.iter().map(latency_ms_f64).collect();
    latencies.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p99_latency = if latencies.is_empty() {
        0.0
    } else {
        let index = latencies.len().saturating_mul(99).div_ceil(100);
        let index = index.saturating_sub(1).min(latencies.len() - 1);
        latencies[index]
    };
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

#[cfg(test)]
mod tests {
    use super::{Period, ReportRow, load_cost_map};
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "mcp_forge_report_test_{}_{}",
            name,
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn load_cost_map_reads_from_given_config_path() {
        let path = unique_temp_path("cost_map");
        std::fs::write(
            &path,
            "[server.github]\ncmd = \"true\"\nestimated_cost_per_call_usd = 0.02\n",
        )
        .unwrap();

        let map = load_cost_map(&path);

        assert_eq!(map.get("github"), Some(&0.02));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_cost_map_returns_empty_map_when_config_missing() {
        let path = unique_temp_path("missing_cost_map");
        let map = load_cost_map(&path);
        assert!(map.is_empty());
    }

    fn make_record(server: &str, result_code: i32) -> forge_core::audit::AuditRecord {
        forge_core::audit::AuditRecord {
            id: format!("{}-{}", server, result_code),
            ts: 0,
            server: server.to_string(),
            tool: "t".to_string(),
            args_hash: "abc".to_string(),
            args_json: None,
            result_code,
            latency_ms: 0,
            latency_us: None,
            error: None,
            session_id: None,
        }
    }

    #[test]
    fn filter_executed_events_drops_all_four_denial_reasons() {
        use forge_core::audit::{
            RESULT_CODE_COST_LIMITED, RESULT_CODE_INJECTION_BLOCKED, RESULT_CODE_POLICY_DENIED,
            RESULT_CODE_RATE_LIMITED,
        };
        let events = vec![
            make_record("github", 0),  // success, kept
            make_record("github", -1), // generic error, kept — it DID reach the tool
            make_record("github", RESULT_CODE_POLICY_DENIED),
            make_record("github", RESULT_CODE_RATE_LIMITED),
            make_record("github", RESULT_CODE_COST_LIMITED),
            make_record("github", RESULT_CODE_INJECTION_BLOCKED),
        ];

        let executed = super::filter_executed_events(&events);

        assert_eq!(executed.len(), 2);
        assert!(
            executed
                .iter()
                .all(|e| e.result_code == 0 || e.result_code == -1)
        );
    }

    #[test]
    fn denial_counts_groups_by_server_and_reason() {
        use forge_core::audit::{
            RESULT_CODE_COST_LIMITED, RESULT_CODE_INJECTION_BLOCKED, RESULT_CODE_POLICY_DENIED,
            RESULT_CODE_RATE_LIMITED,
        };
        let events = vec![
            make_record("github", 0),  // success, not a denial
            make_record("github", -1), // generic error, not a denial
            make_record("github", RESULT_CODE_POLICY_DENIED),
            make_record("github", RESULT_CODE_POLICY_DENIED),
            make_record("github", RESULT_CODE_RATE_LIMITED),
            make_record("postgres", RESULT_CODE_COST_LIMITED),
            make_record("postgres", RESULT_CODE_INJECTION_BLOCKED),
        ];

        let counts = super::denial_counts(&events);

        assert_eq!(
            counts.get(&("github".to_string(), "policy-denied")),
            Some(&2)
        );
        assert_eq!(
            counts.get(&("github".to_string(), "rate-limited")),
            Some(&1)
        );
        assert_eq!(
            counts.get(&("postgres".to_string(), "cost-limited")),
            Some(&1)
        );
        assert_eq!(
            counts.get(&("postgres".to_string(), "injection-blocked")),
            Some(&1)
        );
        assert_eq!(counts.len(), 4);
    }

    #[test]
    fn render_markdown_includes_denials_section() {
        use forge_core::audit::RESULT_CODE_POLICY_DENIED;
        let events = vec![make_record("github", RESULT_CODE_POLICY_DENIED)];
        let rows = vec![];
        let total = ReportRow {
            server: "TOTAL".to_string(),
            calls: 0,
            errors: 0,
            error_rate: 0.0,
            avg_latency: 0.0,
            p99_latency: 0.0,
            cost: 0.0,
        };

        let markdown = super::render_markdown(&Period::Week, &None, &rows, &total, &events);

        assert!(markdown.contains("# Forge Report"));
        assert!(markdown.contains("## Denials by Reason"));
        assert!(markdown.contains("github"));
        assert!(markdown.contains("policy-denied"));
        assert!(markdown.contains("1"));
    }

    #[test]
    fn render_markdown_notes_no_denials_when_none_present() {
        let events: Vec<forge_core::audit::AuditRecord> = vec![];
        let rows = vec![];
        let total = ReportRow {
            server: "TOTAL".to_string(),
            calls: 0,
            errors: 0,
            error_rate: 0.0,
            avg_latency: 0.0,
            p99_latency: 0.0,
            cost: 0.0,
        };

        let markdown = super::render_markdown(&Period::Week, &None, &rows, &total, &events);

        assert!(
            markdown.contains("No denied, rate-limited, cost-limited, or injection-blocked calls")
        );
    }
}
