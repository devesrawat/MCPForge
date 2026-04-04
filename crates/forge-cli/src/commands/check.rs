use anyhow::{Context, Result};
use clap::Args;
use forge_core::config::{ForgeConfig, SecretRef};
use keyring::Entry;
use std::net::TcpListener;
use std::path::PathBuf;

#[derive(Debug, Args)]
#[command(about = "Validate forge.toml configuration")]
pub struct Check {
    #[arg(long, help = "Fix common issues automatically")]
    pub fix: bool,
}

impl Check {
    pub fn run(&self) -> Result<()> {
        let config_path = PathBuf::from("forge.toml");
        if !config_path.exists() {
            anyhow::bail!("no forge.toml found in current directory");
        }

        let mut config = ForgeConfig::load_from_file(&config_path)
            .with_context(|| format!("failed to parse config file {}", config_path.display()))?;

        println!(
            "Checking forge configuration at {}...\n",
            config_path.display()
        );

        if self.fix {
            let fixed = fix_literal_secrets(&mut config, &config_path)?;
            if fixed == 0 {
                println!("Nothing to fix.\n");
            }
        }

        let (error_count, warning_count) = run_checks(&config);

        println!(
            "Result: {} error{}, {} warning{}",
            error_count,
            if error_count == 1 { "" } else { "s" },
            warning_count,
            if warning_count == 1 { "" } else { "s" }
        );

        if error_count > 0 {
            anyhow::bail!(
                "check found {} error{}",
                error_count,
                if error_count == 1 { "" } else { "s" }
            );
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
        for (idx, secret_ref) in server_config.secret.values().enumerate() {
            let n = idx + 1;
            match secret_ref {
                forge_core::config::SecretRef::Env(var) => {
                    if std::env::var(var).is_ok() {
                        println!("    [OK] env secret #{n} resolves");
                    } else {
                        println!("    [ERR] env secret #{n} source not set");
                        error_count += 1;
                    }
                }
                forge_core::config::SecretRef::Keychain(key) => {
                    match keyring::Entry::new("mcp-forge", key) {
                        Ok(entry) => match entry.get_password() {
                            Ok(_) => {
                                println!("    [OK] keychain secret #{n} found")
                            }
                            Err(_) => {
                                println!("    [ERR] keychain secret #{n} not found");
                                error_count += 1;
                            }
                        },
                        Err(_) => {
                            println!("    [WARN] keychain unavailable, cannot verify secret #{n}");
                            warning_count += 1;
                        }
                    }
                }
                forge_core::config::SecretRef::Literal(_) => {
                    println!(
                        "    [WARN] a secret is configured as a literal value (use env or keychain instead)"
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

    if config.proxy.enabled {
        let addr = format!("{}:{}", config.proxy.bind, config.proxy.port);
        match TcpListener::bind(&addr) {
            Ok(listener) => {
                drop(listener);
                println!(
                    "    [OK] port {} bind check passed (availability may change before start)",
                    config.proxy.port
                );
            }
            Err(err) => {
                println!(
                    "    [ERR] port {} is not available ({}): {}",
                    config.proxy.port, addr, err
                );
                error_count += 1;
            }
        }
    }
    println!();

    (error_count, warning_count)
}

/// Migrate all literal secrets to the system keychain and rewrite forge.toml.
/// Returns the number of secrets migrated. Prompts interactively for each value.
fn fix_literal_secrets(config: &mut ForgeConfig, config_path: &PathBuf) -> Result<usize> {
    let mut fixed = 0;

    for (server_name, server_cfg) in config.server.iter_mut() {
        for (env_key, secret_ref) in server_cfg.secret.iter_mut() {
            if let SecretRef::Literal(_) = secret_ref {
                let keychain_name = format!("{}.{}", server_name, env_key);
                println!(
                    "[FIX] server '{}': migrating literal secret '{}' → keychain:{}",
                    server_name, env_key, keychain_name
                );
                let pw = rpassword::prompt_password(format!(
                    "  Enter new value for '{}' (will be stored in keychain, not echoed): ",
                    keychain_name
                ))
                .context("failed to read secret value")?;

                let entry = Entry::new("mcp-forge", &keychain_name)
                    .map_err(|e| anyhow::anyhow!("keychain entry error: {}", e))?;
                entry
                    .set_password(&pw)
                    .map_err(|e| anyhow::anyhow!("failed to store in keychain: {}", e))?;

                *secret_ref = SecretRef::Keychain(keychain_name.clone());
                println!("  [OK] stored in keychain as '{}'", keychain_name);
                fixed += 1;
            }
        }
    }

    if fixed > 0 {
        config
            .save_to_file(config_path)
            .context("failed to save updated forge.toml")?;
        println!("\nUpdated forge.toml: {} literal secret(s) migrated to keychain.\n", fixed);
    }

    Ok(fixed)
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
        assert!(
            result.is_err(),
            "invalid allow glob should fail config parsing"
        );
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
        assert!(
            result.is_err(),
            "invalid deny glob should fail config parsing"
        );
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
        assert!(
            errors > 0,
            "missing env var should produce at least one error"
        );
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
