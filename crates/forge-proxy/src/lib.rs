use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};

pub mod auth;
pub mod sse;

pub mod test_helpers;

use auth::AuthLayer;
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
pub use sse::SessionStore;
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
        u64::try_from(d.year()).unwrap_or(0) * 10_000
            + u64::from(d.month()) * 100
            + u64::from(d.day())
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
    /// Active SSE sessions: session_id → sender for SSE event messages.
    pub sessions: SessionStore,
    /// Optional Bearer token for proxy auth. `None` = auth disabled.
    pub auth_token: Option<String>,
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
            sessions: Arc::new(DashMap::new()),
            auth_token: None,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
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

/// Return a JSON-RPC parse error response body with HTTP 200.
fn parse_error_response() -> Response {
    let body = r#"{"jsonrpc":"2.0","error":{"code":-32700,"message":"Parse error"},"id":null}"#;
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

pub fn build_router(state: ProxyAppState) -> Router {
    let auth_token = state.auth_token.clone();
    Router::new()
        .route("/", post(handle_mcp_request))
        // BUG-09: MCP Streamable HTTP spec discovery endpoint
        .route("/.well-known/mcp", get(handle_well_known_mcp))
        .route("/.well-known/mcp-servers.json", get(handle_well_known))
        // Legacy SSE transport (MCP 2024-11-05 §3.2)
        .route("/sse", get(sse::handle_sse_connect))
        .route("/messages", post(sse::handle_sse_message))
        // Security hardening: 10MB request limit and 60s timeout
        .layer(DefaultBodyLimit::max(10 * 1024 * 1024))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(60),
        ))
        .layer(AuthLayer::new(auth_token))
        .with_state(state)
}

/// BUG-09: GET /.well-known/mcp — MCP Streamable HTTP client auto-discovery.
async fn handle_well_known_mcp() -> impl IntoResponse {
    let body = json!({
        "name": "mcp-forge",
        "version": env!("CARGO_PKG_VERSION"),
        "transport": "http",
        "endpoint": "/",
    });
    (StatusCode::OK, Json(body))
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

/// BUG-10, BUG-15, BUG-16: Handle single and batch JSON-RPC requests with proper
/// content-type checking and notification semantics.
async fn handle_mcp_request(
    State(state): State<ProxyAppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // BUG-10: Validate Content-Type before attempting to parse.
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !content_type
        .split(';')
        .next()
        .map(str::trim)
        .unwrap_or("")
        .eq_ignore_ascii_case("application/json")
    {
        return parse_error_response();
    }

    // BUG-10: Parse as raw Value first so we can detect batch vs single and
    // check key presence for notifications (BUG-16).
    let value: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return parse_error_response(),
    };

    match value {
        // BUG-15: Batch request — process each item and collect non-notification responses.
        Value::Array(items) => {
            // JSON-RPC 2.0 §6: an empty batch array is an invalid request.
            if items.is_empty() {
                let body =
                    serde_json::to_string(&JsonRpcResponse::error(-32600, "Invalid Request", None))
                        .unwrap_or_default();
                return (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/json")],
                    body,
                )
                    .into_response();
            }
            let mut responses: Vec<Value> = Vec::new();
            for item in items {
                if let Some(resp) = process_single_value(&state, item).await {
                    responses.push(serde_json::to_value(resp).unwrap_or(Value::Null));
                }
            }
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                serde_json::to_string(&responses).unwrap_or_else(|_| "[]".to_owned()),
            )
                .into_response()
        }
        // Single request.
        Value::Object(_) => match process_single_value(&state, value).await {
            // BUG-16: Notification — no response body.
            None => StatusCode::OK.into_response(),
            Some(resp) => (StatusCode::OK, Json(resp)).into_response(),
        },
        // Malformed: not an object or array.
        _ => parse_error_response(),
    }
}

/// Process a single JSON-RPC value. Returns `None` for notifications (no `id` key).
async fn process_single_value(state: &ProxyAppState, value: Value) -> Option<JsonRpcResponse> {
    // BUG-16: A notification is a request object with no `id` key at all.
    // `id: null` is a valid request with a null id — check key presence, not value.
    let is_notification = value
        .as_object()
        .map(|o| !o.contains_key("id"))
        .unwrap_or(false);

    // Deserialize into the typed request struct.
    let request: JsonRpcRequest = match serde_json::from_value(value) {
        Ok(r) => r,
        Err(_) => {
            // BUG-10: Return JSON-RPC parse error for malformed request objects.
            return Some(JsonRpcResponse::error(-32700, "Parse error", None));
        }
    };

    // JSON-RPC 2.0 §4: the "jsonrpc" member MUST be exactly "2.0".
    if request.jsonrpc.as_deref() != Some("2.0") {
        return Some(JsonRpcResponse::error(
            -32600,
            "Invalid Request",
            request.id.clone(),
        ));
    }

    let id = request.id.clone();

    let result_resp = match dispatch_request(state, request).await {
        Ok(result) => JsonRpcResponse::success(result, id),
        Err(err) => JsonRpcResponse::error(err.code(), err.to_string(), id),
    };

    // BUG-16: Suppress response for notifications.
    if is_notification {
        None
    } else {
        Some(result_resp)
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
    _params: Option<Value>,
) -> Result<Value, ProxyError> {
    // Fetch all tools across all servers as ToolInfo structs.
    let all_tools = state
        .registry
        .list_all_tools()
        .await
        .map_err(ProxyError::internal)?;

    // BUG-12: Filter tools against allowed_tools / deny_tools policy per server.
    // Tool names are namespaced as `server__tool`; strip the prefix before policy check.
    let filtered_tools: Vec<Value> = all_tools
        .into_iter()
        .filter(|tool_info| {
            let namespaced = &tool_info.name;
            match forge_core::protocol::parse_namespaced_tool(namespaced) {
                Some((server, orig_tool)) => state
                    .policies
                    .get(server)
                    .map(|p| p.is_allowed(orig_tool))
                    .unwrap_or(true),
                // Cannot parse namespace format — keep the tool so it isn't silently dropped.
                None => true,
            }
        })
        .map(|tool_info| {
            let mut tool = json!({
                "name": tool_info.name,
                "inputSchema": tool_info.input_schema,
            });
            if let Some(desc) = &tool_info.description {
                tool["description"] = json!(desc);
            }
            tool
        })
        .collect();

    Ok(json!({ "tools": filtered_tools }))
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

    let (server, orig_tool) = forge_core::protocol::parse_namespaced_tool(tool_name)
        .ok_or_else(|| ProxyError::invalid_params("tool name must be server__tool"))?;

    tracing::Span::current().record("server", server);
    tracing::Span::current().record("tool", orig_tool);

    scan_args_for_injection(state, server, orig_tool, &args)?;

    check_policy_and_guards(state, server, orig_tool, &args)?;

    let start = Instant::now();
    let result = state.registry.call_tool(tool_name, args.clone()).await;
    let latency_us = u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX);
    let latency_ms = latency_us / 1000;
    tracing::Span::current().record("latency_ms", latency_ms);

    if let Ok(ref v) = result {
        scan_result_for_injection(state, server, orig_tool, &args, v)?;
    }

    let (result_code, error) = match &result {
        Ok(_) => (0, None),
        Err(e) => (-1, Some(e.to_string())),
    };
    write_audit_event(
        state,
        server,
        orig_tool,
        &args,
        result_code,
        latency_us,
        error,
    );

    result.map_err(ProxyError::internal)
}

fn scan_args_for_injection(
    state: &ProxyAppState,
    server: &str,
    orig_tool: &str,
    args: &Value,
) -> Result<(), ProxyError> {
    if !state.config.guard.enabled {
        return Ok(());
    }
    let alerts = state.injection_detector.scan_all_arguments(args);
    for alert in &alerts {
        tracing::warn!(
            matched_pattern = alert.matched_pattern,
            position = alert.position,
            "prompt injection detected in tool arguments"
        );
    }
    if !alerts.is_empty() && state.injection_detector.mode() == InjectionMode::Block {
        let err = ProxyError::injection_detected("Potential prompt injection detected in arguments");
        if let Some(aw) = &state.audit {
            aw.log(AuditEvent::new(
                server,
                orig_tool,
                args,
                forge_core::audit::RESULT_CODE_INJECTION_BLOCKED,
                0,
                Some(err.to_string()),
                None,
            ));
        }
        return Err(err);
    }
    Ok(())
}

fn check_policy_and_guards(
    state: &ProxyAppState,
    server: &str,
    orig_tool: &str,
    args: &Value,
) -> Result<(), ProxyError> {
    // RBAC is always enforced, independent of guard.enabled.
    if let Some(policy) = state.policies.get(server)
        && !policy.is_allowed(orig_tool)
    {
        if let Some(aw) = &state.audit {
            aw.log(AuditEvent::new(
                server,
                orig_tool,
                args,
                forge_core::audit::RESULT_CODE_POLICY_DENIED,
                0,
                Some("tool blocked by policy".to_owned()),
                None,
            ));
        }
        return Err(ProxyError::policy_denied(format!(
            "tool '{}' blocked by policy for server '{}'",
            orig_tool, server
        )));
    }

    // Rate limiting and cost guard are always enforced, independent of guard.enabled.
    if let Some(lim) = state.rate_limiters.get(server)
        && lim.check().is_err()
    {
        let err = ProxyError::rate_limited(server);
        if let Some(aw) = &state.audit {
            aw.log(AuditEvent::new(
                server,
                orig_tool,
                args,
                forge_core::audit::RESULT_CODE_RATE_LIMITED,
                0,
                Some(err.to_string()),
                None,
            ));
        }
        return Err(err);
    }

    let srv_cfg = state
        .config
        .server
        .get(server)
        .ok_or_else(|| ProxyError::internal(anyhow::anyhow!("unknown server {}", server)))?;
    if let Err(cost_err) = state.cost_guard.check(server, srv_cfg.max_calls_per_day) {
        let err = ProxyError::cost_limited(cost_err.to_string());
        if let Some(aw) = &state.audit {
            aw.log(AuditEvent::new(
                server,
                orig_tool,
                args,
                forge_core::audit::RESULT_CODE_COST_LIMITED,
                0,
                Some(err.to_string()),
                None,
            ));
        }
        return Err(err);
    }

    Ok(())
}

fn scan_result_for_injection(
    state: &ProxyAppState,
    server: &str,
    orig_tool: &str,
    args: &Value,
    result: &Value,
) -> Result<(), ProxyError> {
    if !state.config.guard.enabled {
        return Ok(());
    }
    if let Some(alert) = state.injection_detector.scan_result(result) {
        tracing::warn!(
            matched_pattern = alert.matched_pattern,
            position = alert.position,
            "prompt injection detected in tool result (indirect injection)"
        );
        if state.injection_detector.mode() == InjectionMode::Block {
            let err =
                ProxyError::injection_detected("Potential prompt injection detected in tool result");
            if let Some(aw) = &state.audit {
                aw.log(AuditEvent::new(
                    server,
                    orig_tool,
                    args,
                    forge_core::audit::RESULT_CODE_INJECTION_BLOCKED,
                    0,
                    Some(err.to_string()),
                    None,
                ));
            }
            return Err(err);
        }
    }
    Ok(())
}

fn write_audit_event(
    state: &ProxyAppState,
    server: &str,
    tool: &str,
    args: &Value,
    result_code: i32,
    latency_us: u64,
    error: Option<String>,
) {
    if let Some(aw) = &state.audit {
        aw.log(AuditEvent::new_with_latency_us(
            server,
            tool,
            args,
            result_code,
            latency_us,
            error,
            None,
        ));
    }
}

#[derive(Debug)]
pub enum ProxyError {
    InvalidParams(String),
    MethodNotFound(String),
    RateLimited(String),
    PolicyDenied(String),
    InjectionDetected(String),
    CostLimited(String),
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

    pub fn cost_limited(message: impl Into<String>) -> Self {
        ProxyError::CostLimited(message.into())
    }

    pub fn internal(error: impl Into<anyhow::Error>) -> Self {
        ProxyError::Internal(error.into())
    }

    pub fn code(&self) -> i32 {
        match self {
            ProxyError::InvalidParams(_) => -32602, // JSON-RPC: Invalid params
            ProxyError::MethodNotFound(_) => -32601, // JSON-RPC: Method not found
            ProxyError::Internal(_) => -32603,      // JSON-RPC: Internal error
            ProxyError::RateLimited(_) => -32000,   // App: rate limited
            ProxyError::PolicyDenied(_) => -32001,  // App: policy denied
            ProxyError::InjectionDetected(_) => -32002, // App: security violation
            ProxyError::CostLimited(_) => -32003,   // App: daily cost/call limit exceeded
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
            ProxyError::CostLimited(m) => write!(f, "{}", m),
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
            .body(Body::from(
                r#"{"jsonrpc":"2.0","method":"tools/list","id":1}"#,
            ))
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
            "jsonrpc": "2.0",
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

    #[tokio::test]
    async fn well_known_mcp_returns_discovery_document() {
        let router = build_router(test_state(ToolRegistry::new(HashMap::new())));
        let request = Request::builder()
            .method("GET")
            .uri("/.well-known/mcp")
            .body(Body::empty())
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        let body_json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body_json["name"], "mcp-forge");
        assert_eq!(body_json["transport"], "http");
        assert_eq!(body_json["endpoint"], "/");
    }

    #[tokio::test]
    async fn parse_error_on_bad_json() {
        let router = build_router(test_state(ToolRegistry::new(HashMap::new())));
        let request = Request::builder()
            .method("POST")
            .uri("/")
            .header("content-type", "application/json")
            .body(Body::from("not-valid-json"))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        let body_json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body_json["error"]["code"], -32700);
        assert_eq!(body_json["error"]["message"], "Parse error");
    }

    #[tokio::test]
    async fn parse_error_on_wrong_content_type() {
        let router = build_router(test_state(ToolRegistry::new(HashMap::new())));
        let request = Request::builder()
            .method("POST")
            .uri("/")
            .header("content-type", "text/plain")
            .body(Body::from(r#"{"method":"tools/list","id":1}"#))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        let body_json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body_json["error"]["code"], -32700);
    }

    #[tokio::test]
    async fn batch_request_returns_array() {
        let mut transports: HashMap<String, Arc<dyn forge_core::mcp::McpTransport>> =
            HashMap::new();
        transports.insert(
            "local".to_string(),
            Arc::new(MockMcpTransport::new(vec!["ping".to_string()])),
        );

        let router = build_router(test_state(ToolRegistry::new(transports)));
        let payload = json!([
            {"jsonrpc": "2.0", "method": "initialize", "id": 1},
            {"jsonrpc": "2.0", "method": "tools/list", "id": 2}
        ]);

        let request = Request::builder()
            .method("POST")
            .uri("/")
            .header("content-type", "application/json")
            .body(Body::from(payload.to_string()))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        let body_json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(body_json.is_array(), "batch response must be array");
        assert_eq!(body_json.as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn notification_returns_empty_200() {
        let router = build_router(test_state(ToolRegistry::new(HashMap::new())));
        // A notification has no `id` field.
        let request = Request::builder()
            .method("POST")
            .uri("/")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"jsonrpc":"2.0","method":"initialize"}"#))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        assert!(
            body.is_empty(),
            "notification must produce empty response body"
        );
    }

    /// BUG-15: Batch with a notification — the notification must be omitted from the array.
    #[tokio::test]
    async fn batch_notification_omitted_from_response() {
        let router = build_router(test_state(ToolRegistry::new(HashMap::new())));
        // Item 0: notification (no `id`), item 1: request (has `id`).
        let payload = json!([
            {"jsonrpc": "2.0", "method": "initialize"},
            {"jsonrpc": "2.0", "method": "initialize", "id": 99}
        ]);
        let request = Request::builder()
            .method("POST")
            .uri("/")
            .header("content-type", "application/json")
            .body(Body::from(payload.to_string()))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        let body_json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let arr = body_json.as_array().expect("batch response must be array");
        assert_eq!(
            arr.len(),
            1,
            "notification must be omitted; only 1 response expected"
        );
        assert_eq!(arr[0]["id"], 99);
    }

    /// BUG-11: Auth rejection must return JSON-RPC error body, not plain text.
    #[tokio::test]
    async fn auth_rejection_returns_jsonrpc_error() {
        let mut state = test_state(ToolRegistry::new(HashMap::new()));
        state.auth_token = Some("secret-token".to_owned());
        let router = build_router(state);

        // Missing Authorization header.
        let request = Request::builder()
            .method("POST")
            .uri("/")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"method":"tools/list","id":1}"#))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let ct = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            ct.contains("application/json"),
            "auth rejection must be application/json"
        );

        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        let body_json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body_json["error"]["code"], -32001);
        assert_eq!(body_json["error"]["message"], "Unauthorized");
        assert_eq!(body_json["jsonrpc"], "2.0");
    }

    /// BUG-11: Correct token must pass through.
    #[tokio::test]
    async fn auth_valid_token_passes_through() {
        let mut transports: HashMap<String, Arc<dyn forge_core::mcp::McpTransport>> =
            HashMap::new();
        transports.insert(
            "local".to_string(),
            Arc::new(MockMcpTransport::new(vec!["ping".to_string()])),
        );
        let mut state = test_state(ToolRegistry::new(transports));
        state.auth_token = Some("my-secret".to_owned());
        let router = build_router(state);

        let request = Request::builder()
            .method("POST")
            .uri("/")
            .header("content-type", "application/json")
            .header("authorization", "Bearer my-secret")
            .body(Body::from(
                r#"{"jsonrpc":"2.0","method":"tools/list","id":1}"#,
            ))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        let body_json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            body_json["error"].is_null(),
            "valid token should reach handler"
        );
    }

    /// BUG-12: tools/list must filter out denied tools and only return allowed ones.
    #[tokio::test]
    async fn tools_list_filters_denied_tools() {
        let mut transports: HashMap<String, Arc<dyn forge_core::mcp::McpTransport>> =
            HashMap::new();
        transports.insert(
            "local".to_string(),
            Arc::new(MockMcpTransport::new(vec![
                "admin_reset".to_string(),
                "safe_query".to_string(),
                "admin_delete".to_string(),
            ])),
        );

        let cfg = ForgeConfig::parse_str(
            r#"
[server.local]
cmd = "true"
deny_tools = ["admin_*"]
"#,
        )
        .expect("config");
        let state = ProxyAppState::new(ToolRegistry::new(transports), cfg, None).expect("state");
        let router = build_router(state);

        let request = Request::builder()
            .method("POST")
            .uri("/")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"jsonrpc":"2.0","method":"tools/list","id":1}"#,
            ))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        let body_json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let tools = body_json["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(
            !names.contains(&"local__admin_reset"),
            "admin_reset must be filtered by deny_tools"
        );
        assert!(
            !names.contains(&"local__admin_delete"),
            "admin_delete must be filtered by deny_tools"
        );
        assert!(
            names.contains(&"local__safe_query"),
            "safe_query must survive deny filter"
        );
    }

    /// BUG-12: tools/list with allowed_tools whitelist only returns listed tools.
    #[tokio::test]
    async fn tools_list_filters_to_allowed_tools_only() {
        let mut transports: HashMap<String, Arc<dyn forge_core::mcp::McpTransport>> =
            HashMap::new();
        transports.insert(
            "local".to_string(),
            Arc::new(MockMcpTransport::new(vec![
                "build".to_string(),
                "test".to_string(),
                "deploy".to_string(),
            ])),
        );

        let cfg = ForgeConfig::parse_str(
            r#"
[server.local]
cmd = "true"
allowed_tools = ["build", "test"]
"#,
        )
        .expect("config");
        let state = ProxyAppState::new(ToolRegistry::new(transports), cfg, None).expect("state");
        let router = build_router(state);

        let request = Request::builder()
            .method("POST")
            .uri("/")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"jsonrpc":"2.0","method":"tools/list","id":1}"#,
            ))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        let body_json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let tools = body_json["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(
            names.contains(&"local__build"),
            "build must be in allowed list"
        );
        assert!(
            names.contains(&"local__test"),
            "test must be in allowed list"
        );
        assert!(
            !names.contains(&"local__deploy"),
            "deploy must be excluded by allowed_tools whitelist"
        );
    }

    /// JSON-RPC 2.0 §6: an empty batch array must return -32600 Invalid Request.
    #[tokio::test]
    async fn empty_batch_returns_invalid_request() {
        let router = build_router(test_state(ToolRegistry::new(HashMap::new())));
        let request = Request::builder()
            .method("POST")
            .uri("/")
            .header("content-type", "application/json")
            .body(Body::from("[]"))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        let body_json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            body_json["error"]["code"], -32600,
            "empty batch must yield Invalid Request"
        );
    }

    /// JSON-RPC 2.0 §4: missing jsonrpc field must return -32600 Invalid Request.
    #[tokio::test]
    async fn missing_jsonrpc_version_returns_invalid_request() {
        let router = build_router(test_state(ToolRegistry::new(HashMap::new())));
        let request = Request::builder()
            .method("POST")
            .uri("/")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"method":"initialize","id":1}"#))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        let body_json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            body_json["error"]["code"], -32600,
            "missing jsonrpc must yield Invalid Request"
        );
        assert_eq!(body_json["id"], 1);
    }

    /// JSON-RPC 2.0 §4: wrong jsonrpc version must return -32600 Invalid Request.
    #[tokio::test]
    async fn wrong_jsonrpc_version_returns_invalid_request() {
        let router = build_router(test_state(ToolRegistry::new(HashMap::new())));
        let request = Request::builder()
            .method("POST")
            .uri("/")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"jsonrpc":"1.0","method":"initialize","id":2}"#,
            ))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        let body_json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            body_json["error"]["code"], -32600,
            "wrong jsonrpc version must yield Invalid Request"
        );
        assert_eq!(body_json["id"], 2);
    }
}
