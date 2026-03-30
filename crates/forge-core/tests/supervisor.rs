use forge_core::config::{ForgeConfig, GuardConfig, ProxyConfig, ServerConfig, Transport};
use forge_core::supervisor::{Supervisor, data_dir, state_file_path};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

#[tokio::test]
async fn start_all_creates_state_file_for_true_server() {
    let temp_home = env::temp_dir().join(format!(
        "mcp_forge_test_{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let prev_home = env::var_os("HOME");
    // SAFETY: test mutates process env; no other test thread reads HOME concurrently.
    unsafe {
        env::set_var("HOME", &temp_home);
    }
    let _ = fs::remove_dir_all(&temp_home);

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
    let logs_dir = data_dir.join("logs");
    assert!(logs_dir.exists());

    let _ = fs::remove_dir_all(&temp_home);
    unsafe {
        match &prev_home {
            Some(h) => env::set_var("HOME", h),
            None => env::remove_var("HOME"),
        }
    }
}
