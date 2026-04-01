use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use chrono::Datelike;
use dashmap::DashMap;
use forge_core::audit::{AuditEvent, AuditWriter};
use forge_core::config::{ForgeConfig, RbacPolicy};
use forge_core::injection::{InjectionDetector, InjectionMode};
use forge_core::mcp::ToolRegistry;
use governor::clock::DefaultClock;
use governor::state::{InMemoryState, NotKeyed};
use governor::{Quota, RateLimiter};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tower_http::timeout::TimeoutLayer;
use tracing::instrument;

pub type SharedLimiter = Arc<RateLimiter<NotKeyed, InMemoryState, DefaultClock>>;

/// Per-server daily call cap (UTC day rollover).
pub struct CostGuard {
    day: AtomicU64,
    counts: DashMap<String, AtomicU64>,
}

impl Default for CostGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl CostGuard {
    pub fn new() -> Self {
        Self {
            day: AtomicU64::new(Self::current_day_key()),
            counts: DashMap::new(),
        }
    }

    fn current_day_key() -> u64 {
        let d = chrono::Utc::now().date_naive();
        d.year() as u64 * 10_000 + d.month() as u64 * 100 + d.day() as u64
    }

    fn roll_day_if_needed(&self) {
        let today = Self::current_day_key();
        let prev = self.day.load(Ordering::SeqCst);
        if today != prev
            && self
                .day
                .compare_exchange(prev, today, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        {
            self.counts.clear();
        }
    }

    pub fn check(&self, server: &str, max_per_day: Option<u32>) -> anyhow::Result<()> {
        let Some(max) = max_per_day else {
            return Ok(());
        };
        self.roll_day_if_needed();
        let entry = self
            .counts
            .entry(server.to_string())
            .or_insert_with(|| AtomicU64::new(0));
        // Increment first, then check: prevents concurrent threads from both
        // reading the same pre-increment value and both passing the limit.
        let new_val = entry.fetch_add(1, Ordering::SeqCst) + 1;
        if new_val > max as u64 {
            entry.fetch_sub(1, Ordering::SeqCst);
            return Err(anyhow::anyhow!(
                "Daily limit exceeded for '{}': max {} calls per day (UTC)",
                server,
                max
            ));
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct ProxyAppState {
    pub registry: Arc<ToolRegistry>,
    pub config: Arc<ForgeConfig>,
    pub audit: Option<Arc<AuditWriter>>,
    pub rate_limiters: Arc<DashMap<String, SharedLimiter>>,
    pub cost_guard: Arc<CostGuard>,
    pub policies: Arc<HashMap<String, RbacPolicy>>,
    pub injection_detector: Arc<InjectionDetector>,
}

impl ProxyAppState {
    pub fn new(
        registry: ToolRegistry,
        config: ForgeConfig,
        audit: Option<Arc<AuditWriter>>,
    ) -> anyhow::Result<Self> {
        let injection_mode = parse_injection_mode(&config.guard.injection_mode)?;

        let rate_limiters = Arc::new(DashMap::new());
        for (name, srv) in &config.server {
            let n = NonZeroU32::new(srv.max_calls_per_min.max(1)).unwrap();
            let lim = Arc::new(RateLimiter::direct(Quota::per_minute(n)));
            rate_limiters.insert(name.clone(), lim);
        }

        let mut policies = HashMap::new();
        for (name, srv) in &config.server {
            policies.insert(
                name.clone(),
                RbacPolicy::from_server_config(srv)
                    .map_err(|e| anyhow::anyhow!("policy compile: {}", e))?,
            );
        }

        Ok(Self {
            registry: Arc::new(registry),
            config: Arc::new(config),
            audit,
            rate_limiters,
            cost_guard: Arc::new(CostGuard::new()),
            policies: Arc::new(policies),
            injection_detector: Arc::new(InjectionDetector::new(injection_mode)),
        })
    }
}

fn parse_injection_mode(mode: &str) -> anyhow::Result<InjectionMode> {
    match mode.trim().to_ascii_lowercase().as_str() {
        "warn" => Ok(InjectionMode::Warn),
        "block" => Ok(InjectionMode::Block),
        other => Err(anyhow::anyhow!(
            "invalid guard.injection_mode '{}'; expected 'warn' or 'block'",
            other
        )),
    }
}

#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: Option<String>,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
    pub id: Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub result: Option<Value>,
    pub error: Option<JsonRpcError>,
    pub id: Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    pub data: Option<Value>,
}

impl JsonRpcResponse {
    pub fn success(result: Value, id: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0".to_owned(),
            result: Some(result),
            error: None,
            id,
        }
    }

    pub fn error(code: i32, message: impl Into<String>, id: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0".to_owned(),
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: None,
            }),
            id,
        }
    }
}

pub fn build_router(state: ProxyAppState) -> Router {
    Router::new()
        .route("/", post(handle_mcp_request))
        .route("/.well-known/mcp-servers.json", get(handle_well_known))
        .route("/sse", get(legacy_sse_info))
        .route("/messages", post(legacy_sse_info))
        // Security hardening: 10MB request limit and 60s timeout
        .layer(DefaultBodyLimit::max(10 * 1024 * 1024))
        .layer(TimeoutLayer::new(Duration::from_secs(60)))
        .with_state(state)
}

async fn handle_well_known(State(state): State<ProxyAppState>) -> impl IntoResponse {
    let mut servers = Vec::new();

    // Map wildcard bind addresses to localhost so clients receive a connectable URL.
    let host = match state.config.proxy.bind.as_str() {
        "0.0.0.0" | "::" => "localhost",
        h => h,
    };

    for (name, config) in &state.config.server {
        let server_info = json!({
            "name": name,
            "transport": "http",
            "endpoint": format!("http://{}:{}/", host, state.config.proxy.port),
            "tags": config.tags,
        });
        servers.push(server_info);
    }

    let response = json!({
        "forge_version": env!("CARGO_PKG_VERSION"),
        "mcp_version": "2024-11-05",
        "servers": servers,
    });

    (StatusCode::OK, Json(response))
}

async fn legacy_sse_info() -> impl IntoResponse {
    (
        StatusCode::NOT_IMPLEMENTED,
        "legacy SSE transport is not implemented; use POST / (streamable HTTP JSON-RPC)",
    )
}

async fn handle_mcp_request(
    State(state): State<ProxyAppState>,
    Json(request): Json<JsonRpcRequest>,
) -> impl IntoResponse {
    let id = request.id.clone();

    match dispatch_request(&state, request).await {
        Ok(result) => (StatusCode::OK, Json(JsonRpcResponse::success(result, id))),
        Err(err) => {
            let response = JsonRpcResponse::error(err.code(), err.to_string(), id);
            (StatusCode::OK, Json(response))
        }
    }
}

async fn dispatch_request(
    state: &ProxyAppState,
    request: JsonRpcRequest,
) -> Result<Value, ProxyError> {
    match request.method.as_str() {
        "initialize" => Ok(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": "mcp-forge",
                "version": env!("CARGO_PKG_VERSION"),
            }
        })),
        "tools/list" => handle_tools_list(state, request.params).await,
        "tools/call" => handle_tools_call(state, request.params).await,
        _ => Err(ProxyError::method_not_found(&request.method)),
    }
}

async fn handle_tools_list(
    state: &ProxyAppState,
    params: Option<Value>,
) -> Result<Value, ProxyError> {
    let tools = if let Some(params) = params {
        if let Some(server) = params.get("server").and_then(Value::as_str) {
            let names = state
                .registry
                .list_tools(server)
                .await
                .map_err(ProxyError::internal)?;
            names
                .into_iter()
                .map(|t| forge_core::protocol::namespace_tool(server, &t))
                .collect::<Vec<_>>()
        } else {
            state
                .registry
                .list_all_tools()
                .await
                .map_err(ProxyError::internal)?
        }
    } else {
        state
            .registry
            .list_all_tools()
            .await
            .map_err(ProxyError::internal)?
    };

    let tool_objs: Vec<Value> = tools
        .into_iter()
        .map(|name| {
            json!({
                "name": name,
                "inputSchema": { "type": "object", "properties": {} }
            })
        })
        .collect();

    Ok(json!({ "tools": tool_objs }))
}

#[instrument(
    skip(state, params),
    fields(method = "tools/call", server = tracing::field::Empty, tool = tracing::field::Empty, latency_ms = tracing::field::Empty)
)]
async fn handle_tools_call(
    state: &ProxyAppState,
    params: Option<Value>,
) -> Result<Value, ProxyError> {
    let params = params.ok_or_else(|| ProxyError::invalid_params("missing params"))?;
    let tool_name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| ProxyError::invalid_params("missing tool name"))?;
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    if state.config.guard.enabled {
        // Scan arguments for prompt injection
        if let Some(alert) = state.injection_detector.scan_arguments(&args) {
            tracing::warn!(
                matched_pattern = alert.matched_pattern,
                position = alert.position,
                "prompt injection detected in tool arguments"
            );

            if state.injection_detector.mode() == InjectionMode::Block {
                return Err(ProxyError::injection_detected(
                    "Potential prompt injection detected in arguments",
                ));
            }
        }
    }

    let (server, orig_tool) = forge_core::protocol::parse_namespaced_tool(tool_name)
        .ok_or_else(|| ProxyError::invalid_params("tool name must be server__tool"))?;

    if let Some(policy) = state.policies.get(server) {
        if !policy.is_allowed(orig_tool) {
            if let Some(audit_writer) = &state.audit {
                let event = AuditEvent::new(
                    server,
                    orig_tool,
                    &args,
                    -403,
                    0,
                    Some("tool blocked by policy".to_owned()),
                    None,
                );
                audit_writer.log(event);
            }
            return Err(ProxyError::policy_denied(format!(
                "tool '{}' blocked by policy for server '{}'",
                orig_tool, server
            )));
        }
    }

    if state.config.guard.enabled {
        if let Some(lim) = state.rate_limiters.get(server) {
            if lim.check().is_err() {
                return Err(ProxyError::rate_limited(server));
            }
        }
    }

    let srv_cfg = state
        .config
        .server
        .get(server)
        .ok_or_else(|| ProxyError::internal(anyhow::anyhow!("unknown server {}", server)))?;

    if state.config.guard.enabled {
        state
            .cost_guard
            .check(server, srv_cfg.max_calls_per_day)
            .map_err(ProxyError::internal)?;
    }

    let start = Instant::now();
    let result = state.registry.call_tool(tool_name, args.clone()).await;
    let latency_ms = start.elapsed().as_millis() as u64;

    tracing::Span::current().record("latency_ms", latency_ms);
    tracing::Span::current().record("server", server);
    tracing::Span::current().record("tool", orig_tool);

    // Scan tool result for indirect prompt injection
    if state.config.guard.enabled {
        if let Ok(ref result_value) = result {
            if let Some(alert) = state.injection_detector.scan_result(result_value) {
                tracing::warn!(
                    matched_pattern = alert.matched_pattern,
                    position = alert.position,
                    "prompt injection detected in tool result (indirect injection)"
                );

                if state.injection_detector.mode() == InjectionMode::Block {
                    return Err(ProxyError::injection_detected(
                        "Potential prompt injection detected in tool result",
                    ));
                }
            }
        }
    }

    if let Some(audit_writer) = &state.audit {
        if let Some((s, t)) = forge_core::protocol::parse_namespaced_tool(tool_name) {
            let error = result.as_ref().err().map(|e| e.to_string());
            let result_code = if result.is_ok() { 0 } else { -1 };
            let event = AuditEvent::new(s, t, &args, result_code, latency_ms, error, None);
            audit_writer.log(event);
        }
    }

    result.map_err(ProxyError::internal)
}

#[derive(Debug)]
pub enum ProxyError {
    InvalidParams(String),
    MethodNotFound(String),
    RateLimited(String),
    PolicyDenied(String),
    InjectionDetected(String),
    Internal(anyhow::Error),
}

impl ProxyError {
    pub fn invalid_params(message: impl Into<String>) -> Self {
        ProxyError::InvalidParams(message.into())
    }

    pub fn method_not_found(method: &str) -> Self {
        ProxyError::MethodNotFound(method.to_owned())
    }

    pub fn rate_limited(server: &str) -> Self {
        ProxyError::RateLimited(server.to_owned())
    }

    pub fn policy_denied(message: impl Into<String>) -> Self {
        ProxyError::PolicyDenied(message.into())
    }

    pub fn injection_detected(message: impl Into<String>) -> Self {
        ProxyError::InjectionDetected(message.into())
    }

    pub fn internal(error: impl Into<anyhow::Error>) -> Self {
        ProxyError::Internal(error.into())
    }

    pub fn code(&self) -> i32 {
        match self {
            ProxyError::InvalidParams(_) => -32602,
            ProxyError::MethodNotFound(_) => -32601,
            ProxyError::RateLimited(_) => -32_000,
            ProxyError::PolicyDenied(_) => -32_000,
            ProxyError::InjectionDetected(_) => -32_000,
            ProxyError::Internal(_) => -32_000,
        }
    }
}

impl std::fmt::Display for ProxyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProxyError::InvalidParams(message) => write!(f, "Invalid params: {}", message),
            ProxyError::MethodNotFound(method) => write!(f, "Method not found: {}", method),
            ProxyError::RateLimited(s) => write!(f, "Rate limit exceeded for server '{}'", s),
            ProxyError::PolicyDenied(m) => write!(f, "{}", m),
            ProxyError::InjectionDetected(m) => write!(f, "Security violation: {}", m),
            ProxyError::Internal(err) => write!(f, "Internal error: {}", err),
        }
    }
}

impl std::error::Error for ProxyError {}

#[cfg(test)]
mod cost_guard_tests {
    use super::*;

    #[test]
    fn enforces_exact_daily_limit() {
        let guard = CostGuard::new();
        for _ in 0..5 {
            assert!(
                guard.check("svc", Some(5)).is_ok(),
                "calls within limit should succeed"
            );
        }
        assert!(
            guard.check("svc", Some(5)).is_err(),
            "call exceeding daily limit should fail"
        );
    }

    #[test]
    fn no_limit_is_unrestricted() {
        let guard = CostGuard::new();
        for _ in 0..1_000 {
            assert!(guard.check("svc", None).is_ok());
        }
    }

    #[test]
    fn limits_are_per_server() {
        let guard = CostGuard::new();
        for _ in 0..3 {
            assert!(guard.check("alpha", Some(3)).is_ok());
            assert!(guard.check("beta", Some(3)).is_ok());
        }
        assert!(guard.check("alpha", Some(3)).is_err());
        assert!(guard.check("beta", Some(3)).is_err());
    }

    #[test]
    fn concurrent_calls_never_exceed_limit() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        const LIMIT: u32 = 10;
        const THREADS: usize = 100;

        let guard = Arc::new(CostGuard::new());
        let successes = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let g = guard.clone();
                let s = successes.clone();
                std::thread::spawn(move || {
                    if g.check("svc", Some(LIMIT)).is_ok() {
                        s.fetch_add(1, Ordering::SeqCst);
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        let total = successes.load(Ordering::SeqCst);
        assert_eq!(
            total, LIMIT as usize,
            "exactly LIMIT={} calls should succeed under concurrent load, got {}",
            LIMIT, total
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use forge_core::mcp::{MockMcpTransport, ToolRegistry};
    use serde_json::json;
    use std::collections::HashMap;
    use tower::util::ServiceExt;

    fn test_state(registry: ToolRegistry) -> ProxyAppState {
        let mut cfg = ForgeConfig::parse_str(
            r#"
[server.local]
cmd = "true"
"#,
        )
        .expect("config");
        cfg.server.get_mut("local").unwrap().max_calls_per_min = 60;
        ProxyAppState::new(registry, cfg, None).expect("state")
    }

    #[tokio::test]
    async fn tools_list_returns_namespaced_tools() {
        let mut transports: HashMap<String, Arc<dyn forge_core::mcp::McpTransport>> =
            HashMap::new();
        transports.insert(
            "local".to_string(),
            Arc::new(MockMcpTransport::new(vec![
                "build".to_string(),
                "test".to_string(),
            ])),
        );

        let router = build_router(test_state(ToolRegistry::new(transports)));
        let request = Request::builder()
            .method("POST")
            .uri("/")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"method":"tools/list","id":1}"#))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        let response_json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        let tools = response_json["result"]["tools"].as_array().unwrap();
        let names: Vec<String> = tools
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["local__build", "local__test"]);
    }

    #[tokio::test]
    async fn tools_call_returns_tool_result() {
        let mut transports: HashMap<String, Arc<dyn forge_core::mcp::McpTransport>> =
            HashMap::new();
        transports.insert(
            "local".to_string(),
            Arc::new(MockMcpTransport::new(vec!["build".to_string()])),
        );

        let router = build_router(test_state(ToolRegistry::new(transports)));

        let payload = json!({
            "method": "tools/call",
            "params": {
                "name": "local__build",
                "arguments": { "task": "compile" }
            },
            "id": 42
        });

        let request = Request::builder()
            .method("POST")
            .uri("/")
            .header("content-type", "application/json")
            .body(Body::from(payload.to_string()))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        let response_json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(response_json["result"]["tool"], "build");
        assert_eq!(response_json["result"]["args"]["task"], "compile");
    }
}
