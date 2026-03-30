use forge_core::config::{ForgeConfig, Transport};

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
fn reject_unknown_field() {
    let manifest = r#"
[server.github]
cmd = "echo status"
typo = true
"#;

    assert!(toml::from_str::<ForgeConfig>(manifest).is_err());
}
