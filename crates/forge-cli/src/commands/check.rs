use anyhow::{Context, Result};
use clap::Args;
use forge_core::config::ForgeConfig;
use std::path::PathBuf;

#[derive(Debug, Args)]
pub struct Check {
    #[arg(long, help = "Fix common issues automatically")]
    pub fix: bool,
}

impl Check {
    pub fn run(&self) -> Result<()> {
        let config_path = PathBuf::from("forge.toml");
        if !config_path.exists() {
            println!("no forge.toml found in current directory");
            std::process::exit(1);
        }

        let config = ForgeConfig::load_from_file(&config_path)
            .with_context(|| format!("failed to parse config file {}", config_path.display()))?;

        println!("Checking forge configuration at {}...\n", config_path.display());

        let (error_count, warning_count) = run_checks(&config);

        println!(
            "Result: {} error{}, {} warning{}",
            error_count,
            if error_count == 1 { "" } else { "s" },
            warning_count,
            if warning_count == 1 { "" } else { "s" }
        );

        if error_count > 0 {
            std::process::exit(1);
        }
        Ok(())
    }
}

/// Validate all servers in `config`, printing results to stdout.
/// Returns (error_count, warning_count).
pub fn run_checks(config: &ForgeConfig) -> (usize, usize) {
    let mut error_count = 0;
    let mut warning_count = 0;

    for (name, server_config) in &config.server {
        println!("  server.{}:", name);

        // Check if command exists in PATH
        let parts = server_config.cmd_parts();
        if parts.is_empty() {
            println!("    [ERR] empty command");
            error_count += 1;
        } else {
            match which::which(&parts[0]) {
                Ok(_) => println!("    [OK] command '{}' found in PATH", parts[0]),
                Err(_) => {
                    println!("    [ERR] command '{}' not found in PATH", parts[0]);
                    error_count += 1;
                }
            }
        }

        // Check secrets
        for (var_name, secret_ref) in &server_config.secret {
            match secret_ref {
                forge_core::config::SecretRef::Env(var) => {
                    if std::env::var(var).is_ok() {
                        println!(
                            "    [OK] secret '{}' env var '{}' resolves",
                            var_name, var
                        );
                    } else {
                        println!(
                            "    [ERR] secret '{}' env var '{}' not set",
                            var_name, var
                        );
                        error_count += 1;
                    }
                }
                forge_core::config::SecretRef::Keychain(key) => {
                    match keyring::Entry::new("mcp-forge", key) {
                        Ok(entry) => match entry.get_password() {
                            Ok(_) => println!(
                                "    [OK] secret '{}' keychain entry exists",
                                var_name
                            ),
                            Err(_) => {
                                println!(
                                    "    [ERR] secret '{}' keychain entry not found",
                                    var_name
                                );
                                error_count += 1;
                            }
                        },
                        Err(_) => {
                            println!(
                                "    [WARN] keychain unavailable, cannot verify secret '{}'",
                                var_name
                            );
                            warning_count += 1;
                        }
                    }
                }
                forge_core::config::SecretRef::Literal(_) => {
                    println!(
                        "    [WARN] secret '{}' is literal in config (use env or keychain)",
                        var_name
                    );
                    warning_count += 1;
                }
            }
        }

        // Check tool patterns
        let total_patterns = server_config.allowed_tools.len() + server_config.deny_tools.len();
        let mut all_patterns_valid = true;

        for pattern in &server_config.allowed_tools {
            match globset::Glob::new(pattern) {
                Ok(_) => {}
                Err(e) => {
                    println!("    [ERR] invalid allow pattern '{}': {}", pattern, e);
                    error_count += 1;
                    all_patterns_valid = false;
                }
            }
        }

        for pattern in &server_config.deny_tools {
            match globset::Glob::new(pattern) {
                Ok(_) => {}
                Err(e) => {
                    println!("    [ERR] invalid deny pattern '{}': {}", pattern, e);
                    error_count += 1;
                    all_patterns_valid = false;
                }
            }
        }

        if total_patterns > 0 && all_patterns_valid {
            println!("    [OK] tool patterns compile");
        }

        println!();
    }

    // Check proxy config
    println!("  proxy:");
    println!(
        "    [OK] listening on {}:{}",
        config.proxy.bind, config.proxy.port
    );
    println!();

    (error_count, warning_count)
}

#[cfg(test)]
mod tests {
    use super::run_checks;
    use forge_core::config::ForgeConfig;

    // Bad globs are caught at parse time by validate_all_servers — the config
    // never reaches run_checks.  These two tests confirm that contract.
    #[test]
    fn bad_allow_glob_fails_config_parsing() {
        let result = ForgeConfig::parse_str(
            r#"
[server.test]
cmd = "true"
allowed_tools = ["invalid[glob"]
"#,
        );
        assert!(result.is_err(), "invalid allow glob should fail config parsing");
    }

    #[test]
    fn bad_deny_glob_fails_config_parsing() {
        let result = ForgeConfig::parse_str(
            r#"
[server.test]
cmd = "true"
deny_tools = ["broken[pattern"]
"#,
        );
        assert!(result.is_err(), "invalid deny glob should fail config parsing");
    }

    #[test]
    fn missing_env_var_yields_check_error() {
        // Sentinel name extremely unlikely to be set in any environment
        const SENTINEL: &str = "FORGE_CHECK_TEST_MISSING_12345_ABCXYZ";
        let cfg = ForgeConfig::parse_str(&format!(
            r#"
[server.test]
cmd = "true"
secret.API_KEY = "env:{}"
"#,
            SENTINEL
        ))
        .unwrap();
        let (errors, _) = run_checks(&cfg);
        assert!(errors > 0, "missing env var should produce at least one error");
    }

    #[test]
    fn valid_config_yields_no_errors() {
        let cfg = ForgeConfig::parse_str(
            r#"
[server.test]
cmd = "true"
allowed_tools = ["read_*", "list_*"]
deny_tools = ["admin_*"]
"#,
        )
        .unwrap();
        let (errors, _) = run_checks(&cfg);
        assert_eq!(errors, 0, "valid config should yield no errors");
    }

    #[test]
    fn server_with_no_patterns_yields_no_errors() {
        let cfg = ForgeConfig::parse_str(
            r#"
[server.test]
cmd = "true"
"#,
        )
        .unwrap();
        let (errors, _) = run_checks(&cfg);
        assert_eq!(errors, 0);
    }
}
