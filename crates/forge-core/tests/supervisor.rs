use forge_core::config::{ForgeConfig, GuardConfig, ProxyConfig, ServerConfig, Transport};
use forge_core::supervisor::{Supervisor, data_dir, state_file_path};
use std::collections::HashMap;
use std::fs;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Serializes tests that mutate process environment variables.
/// Even though FORGE_HOME is Forge-specific, `std::env::set_var` is `unsafe`
/// in Rust ≥ 1.80 because other threads (e.g., the async runtime) may be
/// reading the environment concurrently.  A global mutex ensures only one test
/// mutates env at a time.
fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

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

    // Hold the global env mutex while setting environment variables, but drop
    // it before the await point to avoid holding a lock across an await
    // (which is unsafe in async contexts).
    {
        let _env_guard = env_lock().lock().unwrap();

        // SAFETY: protected by `env_lock()` above — only one thread mutates env
        // at a time.  FORGE_HOME is Forge-private and not read by any system
        // library, so this cannot trigger the UB that makes HOME mutation unsafe.
        unsafe { std::env::set_var("FORGE_HOME", &temp_forge_home) };
    } // _env_guard is dropped here before the await point

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
                max_restarts: None,
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
    // SAFETY: FORGE_HOME uniqueness ensures tests don't interfere; release the
    // env without holding a lock to avoid async-safety issues.
    unsafe { std::env::remove_var("FORGE_HOME") };
}
