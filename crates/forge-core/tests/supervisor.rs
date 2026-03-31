use forge_core::config::{ForgeConfig, GuardConfig, ProxyConfig, ServerConfig, Transport};
use forge_core::supervisor::{Supervisor, data_dir, state_file_path};
use std::collections::HashMap;
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

#[tokio::test]
async fn start_all_creates_state_file_for_true_server() {
    // Use FORGE_HOME to redirect all forge data to a throwaway temp directory.
    // This avoids unsafe HOME mutation while keeping the test fully isolated.
    let temp_forge_home = std::env::temp_dir().join(format!(
        "mcp_forge_test_{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = fs::remove_dir_all(&temp_forge_home);

    // SAFETY: FORGE_HOME is forge-specific and not consulted by any system
    // library, so mutating it here cannot trigger the undefined behaviour
    // that makes HOME mutation dangerous.  The unique nanosecond suffix
    // prevents collisions between parallel test runs.
    unsafe { std::env::set_var("FORGE_HOME", &temp_forge_home) };

    let config = ForgeConfig {
        server: vec![(
            "test".to_owned(),
            ServerConfig {
                cmd: "true".to_owned(),
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
            },
        )]
        .into_iter()
        .collect(),
        guard: GuardConfig::default(),
        proxy: ProxyConfig::default(),
    };

    let mut supervisor = Supervisor::new(config).expect("Supervisor constructs");
    supervisor
        .start_all()
        .await
        .expect("start_all should succeed");

    let state_path = state_file_path().expect("state file path available");
    let contents = fs::read_to_string(&state_path).expect("state file readable");
    assert!(contents.contains("\"test\""));

    let data_dir = data_dir().expect("data dir available");
    assert!(data_dir.exists());
    assert!(data_dir.join("logs").exists());

    // Cleanup
    let _ = fs::remove_dir_all(&temp_forge_home);
    // SAFETY: same as set_var above — FORGE_HOME is forge-private.
    unsafe { std::env::remove_var("FORGE_HOME") };
}
