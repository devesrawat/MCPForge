use anyhow::Context;
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub mod policy;
pub mod secret;
pub mod validation;

pub use policy::{RbacPolicy, validate_all_servers};
pub use secret::{DefaultSecretResolver, SecretRef, SecretResolver};
pub use validation::{ValidationError, validate_server_name};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ForgeConfig {
    #[serde(default)]
    pub server: HashMap<String, ServerConfig>,
    #[serde(default)]
    pub guard: GuardConfig,
    #[serde(default)]
    pub proxy: ProxyConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub cmd: String,

    #[serde(default = "default_transport")]
    pub transport: Transport,

    #[serde(default)]
    pub secret: HashMap<String, SecretRef>,

    #[serde(default)]
    pub allowed_tools: Vec<String>,

    #[serde(default)]
    pub deny_tools: Vec<String>,

    #[serde(default = "default_rate_limit")]
    pub max_calls_per_min: u32,

    #[serde(default)]
    pub max_calls_per_day: Option<u32>,

    #[serde(default)]
    pub tags: Vec<String>,

    #[serde(default)]
    pub env: HashMap<String, String>,

    #[serde(default)]
    pub ready_timeout_secs: Option<u64>,

    /// Optional estimated USD cost per tool call (for `forge report`).
    #[serde(default)]
    pub estimated_cost_per_call_usd: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GuardConfig {
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyConfig {
    #[serde(default)]
    pub enabled: bool,

    #[serde(default = "default_proxy_bind")]
    pub bind: String,

    #[serde(default = "default_proxy_port")]
    pub port: u16,
}

fn default_proxy_bind() -> String {
    "127.0.0.1".to_owned()
}

fn default_proxy_port() -> u16 {
    3456
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: default_proxy_bind(),
            port: default_proxy_port(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Stdio,
    Http,
}

fn default_transport() -> Transport {
    Transport::Stdio
}

fn default_rate_limit() -> u32 {
    60
}

impl ForgeConfig {
    pub fn load_from_file(path: impl AsRef<std::path::Path>) -> anyhow::Result<Self> {
        let contents = std::fs::read_to_string(path.as_ref())?;
        let config: ForgeConfig = toml::from_str(&contents)?;
        validate_all_servers(&config.server)
            .map_err(|e| anyhow::anyhow!("invalid config {}: {}", path.as_ref().display(), e))?;
        Ok(config)
    }

    pub fn parse_str(contents: &str) -> anyhow::Result<Self> {
        let config: ForgeConfig = toml::from_str(contents)?;
        validate_all_servers(&config.server)?;
        Ok(config)
    }

    pub fn save_to_file(&self, path: impl AsRef<std::path::Path>) -> anyhow::Result<()> {
        let text = toml::to_string_pretty(self)?;
        std::fs::write(path.as_ref(), text)?;
        Ok(())
    }
}

impl ServerConfig {
    pub fn cmd_parts(&self) -> Vec<String> {
        shell_words::split(&self.cmd).unwrap_or_else(|_| {
            self.cmd
                .split_whitespace()
                .map(|part| part.to_owned())
                .collect()
        })
    }
}

pub async fn resolve_server_env(config: &ServerConfig) -> anyhow::Result<HashMap<String, String>> {
    let resolver = DefaultSecretResolver;
    let mut env_vars = config.env.clone();
    for (key, secret_ref) in &config.secret {
        let value = resolver
            .resolve(&config.cmd, secret_ref)
            .await
            .with_context(|| format!("failed to resolve secret '{}'", key))?;
        env_vars.insert(key.clone(), value.expose_secret().to_owned());
    }
    Ok(env_vars)
}
