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

    assert_eq!(server.cmd, "echo status");
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
        cmd: "echo \"hello world\"".to_owned(),
        transport: Transport::Stdio,
        secret: HashMap::new(),
        allowed_tools: Vec::new(),
        deny_tools: Vec::new(),
        max_calls_per_min: 60,
        max_calls_per_day: None,
        tags: Vec::new(),
        env: HashMap::new(),
        ready_timeout_secs: None,
        estimated_cost_per_call_usd: None,
    };

    let parts = config.cmd_parts();
    assert_eq!(parts, vec!["echo", "hello world"]);
}
