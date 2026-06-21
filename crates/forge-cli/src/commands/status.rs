use anyhow::{Context, Result};
use chrono::{TimeZone, Utc};
use clap::Args;
use forge_core::config::ForgeConfig;
use forge_core::supervisor::{PersistentState, ServerState, state_file_path};
use serde_json;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

#[derive(Debug, Args)]
#[command(about = "Show running status of all MCP servers")]
pub struct Status {
    #[arg(long, help = "Update status every 2 seconds")]
    pub watch: bool,

    #[arg(long, help = "Emit JSON instead of table output")]
    pub json: bool,

    #[arg(
        long,
        default_value = "forge.toml",
        help = "Path to the forge config file (used to scope output to local servers)"
    )]
    pub config: PathBuf,
}

/// Returns `true` if the process with the given PID is alive.
///
/// On Unix we probe with signal 0 (no signal is delivered; the kernel just
/// checks whether the process exists and we have permission to signal it).
/// On Windows we cannot do this without Win32 APIs, so we conservatively
/// report the process as alive to preserve the existing behaviour.
#[cfg(unix)]
fn is_pid_alive(pid: u32) -> bool {
    // SAFETY: kill(pid, 0) does not deliver a signal; it only checks
    // whether the process exists. The return value is either 0 (alive / no
    // permission — we treat "no permission" as alive to avoid false negatives)
    // or -1 with errno == ESRCH (process does not exist).
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(not(unix))]
fn is_pid_alive(_pid: u32) -> bool {
    true
}

/// Determine the effective status string for a server entry.
///
/// If the state says "running" but the stored PID is no longer alive, the
/// server has crashed without forge updating state; we report "stopped" instead.
fn effective_status(info: &ServerState) -> &str {
    if info.status == "running" {
        if let Some(pid) = info.pid {
            if !is_pid_alive(pid) {
                return "stopped";
            }
        }
    }
    &info.status
}

/// Try to load the set of server names defined in the local forge.toml.
///
/// Returns:
/// - `Ok(Some(names))` — config loaded, names extracted.
/// - `Ok(None)`        — config file not found; caller should fall back to global view.
/// - `Err(_)`          — config file exists but could not be parsed.
fn local_server_names(config_path: &PathBuf) -> Result<Option<std::collections::HashSet<String>>> {
    if !config_path.exists() {
        return Ok(None);
    }
    let config = ForgeConfig::load_from_file(config_path)
        .with_context(|| format!("failed to parse config file {}", config_path.display()))?;
    let names: std::collections::HashSet<String> = config.server.keys().cloned().collect();
    Ok(Some(names))
}

impl Status {
    pub fn run(&self) -> Result<()> {
        if self.watch {
            loop {
                self.print_status()?;
                thread::sleep(Duration::from_secs(2));
            }
        } else {
            self.print_status()
        }
    }

    fn print_status(&self) -> Result<()> {
        let path = state_file_path()?;
        let contents = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if self.json {
                    println!("{}", serde_json::json!({ "status": "not running" }));
                } else {
                    println!("forge is not running (no state file found)");
                }
                return Ok(());
            }
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("failed to read state file {}", path.display()));
            }
        };

        let state: PersistentState = serde_json::from_str(&contents)
            .with_context(|| format!("failed to parse state file {}", path.display()))?;

        // Determine which servers to show.
        // `is_global_fallback` is set when the local config doesn't exist — we
        // display everything from state.json and add a note in table mode.
        let (filtered_servers, is_global_fallback) =
            self.filter_servers(state.servers)?;

        if self.json {
            // Rebuild the state with only the filtered servers so JSON output
            // is consistent with table output and does not leak other projects.
            let output_state = PersistentState {
                started_at_secs: state.started_at_secs,
                servers: filtered_servers,
            };
            println!("{}", serde_json::to_string_pretty(&output_state)?);
            return Ok(());
        }

        if is_global_fallback {
            println!(
                "note: no forge.toml found at {} — showing all servers from global state\n",
                self.config.display()
            );
        }

        let started_human = Utc
            .timestamp_opt(state.started_at_secs as i64, 0)
            .single()
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_else(|| state.started_at_secs.to_string());
        println!("STARTED {}", started_human);
        println!(
            "{:<18} {:<10} {:<8} {:<10} {:<9} LAST_ERROR",
            "NAME", "STATUS", "PID", "UPTIME", "RESTARTS"
        );
        for (name, info) in &filtered_servers {
            let status = effective_status(info);
            let pid = info
                .pid
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".to_owned());
            let uptime = info
                .uptime_secs
                .map(|v| format!("{}s", v))
                .unwrap_or_else(|| "-".to_owned());
            let last_error = info.last_error.as_deref().unwrap_or("-");
            println!(
                "{:<18} {:<10} {:<8} {:<10} {:<9} {}",
                name, status, pid, uptime, info.restarts, last_error
            );
        }

        Ok(())
    }

    /// Filter the servers map to only entries that belong to the local config.
    ///
    /// Returns `(servers, is_global_fallback)`.
    fn filter_servers(
        &self,
        all_servers: HashMap<String, ServerState>,
    ) -> Result<(HashMap<String, ServerState>, bool)> {
        match local_server_names(&self.config)? {
            Some(names) => {
                let filtered = all_servers
                    .into_iter()
                    .filter(|(name, _)| names.contains(name))
                    .collect();
                Ok((filtered, false))
            }
            None => {
                // No local config — fall back to global view.
                Ok((all_servers, true))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{effective_status, is_pid_alive};
    use forge_core::supervisor::ServerState;

    #[test]
    fn current_process_is_alive() {
        let pid = std::process::id();
        assert!(is_pid_alive(pid), "current process should be alive");
    }

    #[test]
    fn nonexistent_pid_is_dead() {
        // PID 999_999_999 is virtually guaranteed not to exist on any real system.
        // On Windows is_pid_alive always returns true, so skip the assertion there.
        #[cfg(unix)]
        assert!(
            !is_pid_alive(999_999_999),
            "non-existent PID should be reported as dead"
        );
    }

    fn make_server_state(status: &str, pid: Option<u32>) -> ServerState {
        ServerState {
            status: status.to_owned(),
            pid,
            uptime_secs: None,
            restarts: 0,
            last_error: None,
            url: None,
            transport: None,
        }
    }

    #[test]
    fn effective_status_passes_through_non_running() {
        let stopped = make_server_state("stopped", None);
        assert_eq!(effective_status(&stopped), "stopped");

        let degraded = make_server_state("degraded", None);
        assert_eq!(effective_status(&degraded), "degraded");
    }

    #[test]
    fn effective_status_running_with_live_pid_stays_running() {
        let pid = std::process::id();
        let info = make_server_state("running", Some(pid));
        assert_eq!(effective_status(&info), "running");
    }

    #[test]
    #[cfg(unix)]
    fn effective_status_running_with_dead_pid_becomes_stopped() {
        let info = make_server_state("running", Some(999_999_999));
        assert_eq!(effective_status(&info), "stopped");
    }
}
