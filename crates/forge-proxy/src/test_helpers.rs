//! Shared helpers for integration tests in `tests/`.

use forge_core::config::ForgeConfig;
use forge_core::mcp::{MockMcpTransport, McpTransport, ToolRegistry};
use std::collections::HashMap;
use std::sync::Arc;

use crate::ProxyAppState;

pub fn make_state_with_registry(registry: ToolRegistry) -> ProxyAppState {
    let cfg = ForgeConfig::parse_str(
        r#"
[server.test]
cmd = "true"
"#,
    )
    .expect("config parse");
    ProxyAppState::new(registry, cfg, None).expect("state")
}

pub fn make_state_with_registry_and_rate_limit(
    registry: ToolRegistry,
    server: &str,
    max_calls_per_min: u32,
) -> ProxyAppState {
    let cfg = ForgeConfig::parse_str(&format!(
        r#"
[guard]
enabled = true

[server.{server}]
cmd = "true"
max_calls_per_min = {max_calls_per_min}
"#
    ))
    .expect("config parse");
    ProxyAppState::new(registry, cfg, None).expect("state")
}

pub fn make_state_with_auth(token: &str) -> ProxyAppState {
    let mut transports: HashMap<String, Arc<dyn McpTransport>> = HashMap::new();
    transports.insert(
        "test".to_string(),
        Arc::new(MockMcpTransport::new(vec!["ping".to_string()])),
    );
    let registry = ToolRegistry::new(transports);
    let cfg = ForgeConfig::parse_str(
        r#"
[server.test]
cmd = "true"
"#,
    )
    .expect("config parse");
    let mut state = ProxyAppState::new(registry, cfg, None).expect("state");
    state.auth_token = Some(token.to_owned());
    state
}
