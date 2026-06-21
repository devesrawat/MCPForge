use forge_core::config::{ForgeConfig, ServerConfig, Transport};
use proptest::prelude::*;
use std::collections::HashMap;

proptest! {
    #[test]
    fn config_parse_never_panics(body in prop::string::string_regex("[a-zA-Z0-9_=\\.\\[\\]\"\\n #\\-]{0,200}").unwrap()) {
        let wrapped = format!("[server.p]\ncmd = \"echo x\"\n{body}");
        let _ = ForgeConfig::parse_str(&wrapped);
    }
}

#[test]
fn parse_minimal_config() {
    let manifest = r#"
[server.github]
cmd = "echo status"
"#;

    let config: ForgeConfig = toml::from_str(manifest).expect("should parse minimal config");
    let server = config.server.get("github").expect("server github exists");

    assert_eq!(server.cmd.as_deref(), Some("echo status"));
    assert_eq!(server.transport, Transport::Stdio);
    assert!(server.secret.is_empty());
    assert_eq!(server.max_calls_per_min, 60);
}

#[test]
fn reject_invalid_glob() {
    let manifest = r#"
[server.x]
cmd = "true"
allowed_tools = ["["]
"#;
    assert!(ForgeConfig::parse_str(manifest).is_err());
}

#[test]
fn reject_unknown_field() {
    let manifest = r#"
[server.github]
cmd = "echo status"
typo = true
"#;

    assert!(toml::from_str::<ForgeConfig>(manifest).is_err());
}

#[test]
fn parse_cmd_parts_with_quotes() {
    let config = ServerConfig {
        cmd: Some("echo \"hello world\"".to_owned()),
        transport: Transport::Stdio,
        url: None,
        secret: HashMap::new(),
        allowed_tools: Vec::new(),
        deny_tools: Vec::new(),
        max_calls_per_min: 60,
        max_calls_per_day: None,
        tags: Vec::new(),
        env: HashMap::new(),
        ready_timeout_secs: None,
        estimated_cost_per_call_usd: None,
        max_restarts: None,
    };

    let parts = config.cmd_parts();
    assert_eq!(parts, vec!["echo", "hello world"]);
}

#[test]
fn test_config_sse_transport_parses_correctly() {
    let cfg = ForgeConfig::parse_str(
        r#"
[server.linear]
transport = "sse"
url = "https://mcp.linear.app/sse"
"#,
    )
    .unwrap();
    assert_eq!(cfg.server["linear"].transport, Transport::Sse);
    assert_eq!(
        cfg.server["linear"].url.as_deref(),
        Some("https://mcp.linear.app/sse")
    );
}

#[test]
fn test_config_http_without_url_fails_validation() {
    let result = ForgeConfig::parse_str(
        r#"
[server.github]
transport = "http"
"#,
    );
    assert!(result.is_err(), "http transport without url should fail");
    assert!(result.unwrap_err().to_string().contains("url"));
}

#[test]
fn test_config_stdio_without_cmd_fails_validation() {
    let result = ForgeConfig::parse_str(
        r#"
[server.local]
transport = "stdio"
"#,
    );
    assert!(result.is_err(), "stdio transport without cmd should fail");
}

#[test]
fn serialize_minimal_stdio_server_omits_defaults() {
    let mut servers = HashMap::new();
    servers.insert(
        "github".to_owned(),
        ServerConfig {
            cmd: Some("echo status".to_owned()),
            transport: Transport::Stdio,
            url: None,
            secret: HashMap::new(),
            allowed_tools: Vec::new(),
            deny_tools: Vec::new(),
            max_calls_per_min: 60,
            max_calls_per_day: None,
            tags: Vec::new(),
            env: HashMap::new(),
            ready_timeout_secs: None,
            estimated_cost_per_call_usd: None,
            max_restarts: None,
        },
    );
    let cfg = ForgeConfig {
        server: servers,
        guard: Default::default(),
        proxy: Default::default(),
    };
    let text = toml::to_string_pretty(&cfg).unwrap();
    assert!(text.contains("cmd = \"echo status\""));
    assert!(!text.contains("transport"));
    assert!(!text.contains("allowed_tools"));
    assert!(!text.contains("deny_tools"));
    assert!(!text.contains("max_calls_per_min"));
    assert!(!text.contains("[server.github.secret]"));
    assert!(!text.contains("[server.github.env]"));
}

#[test]
fn test_config_http_with_url_passes() {
    let cfg = ForgeConfig::parse_str(
        r#"
[server.github]
transport = "http"
url = "https://api.github.com/mcp"
"#,
    )
    .unwrap();
    assert!(cfg.server["github"].url.is_some());
}
