use anyhow::{Result, anyhow};
use async_trait::async_trait;
use dashmap::DashMap;
use http::{HeaderName, HeaderValue};
use rmcp::{
    RoleClient, ServiceExt,
    model::{CallToolRequestParams, JsonObject},
    service::RunningService,
    transport::streamable_http_client::{
        StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
    },
    transport::{ConfigureCommandExt, TokioChildProcess},
};
use secrecy::ExposeSecret;
use serde_json::Value;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{Mutex, oneshot};
use tracing::warn;

use crate::config::{
    DefaultSecretResolver, ForgeConfig, SecretResolver, ServerConfig, Transport, resolve_server_env,
};

/// Per-server tool list cache: server name → (cached_at, tools).
type PerServerCache = Arc<DashMap<String, (Instant, Vec<ToolInfo>)>>;

/// Set of server names currently undergoing a background cache refresh.
type RefreshingSet = Arc<DashMap<String, ()>>;

/// Tool metadata returned by upstream MCP servers.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolInfo {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
}

#[async_trait]
pub trait McpTransport: Send + Sync {
    async fn list_tools(&self) -> Result<Vec<ToolInfo>>;
    async fn call_tool(&self, name: &str, args: Value) -> Result<Value>;
}

#[derive(Debug, Clone)]
pub struct MockMcpTransport {
    pub tools: Arc<Vec<ToolInfo>>,
    pub call_count: Arc<std::sync::atomic::AtomicUsize>,
}

impl MockMcpTransport {
    /// Convenience constructor: names only, empty input schemas.
    pub fn new<T: Into<Vec<String>>>(names: T) -> Self {
        let tools = names
            .into()
            .into_iter()
            .map(|name| ToolInfo {
                name,
                description: None,
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
            })
            .collect();
        MockMcpTransport {
            tools: Arc::new(tools),
            call_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// Constructor with full schema information.
    pub fn with_schemas(tools: Vec<ToolInfo>) -> Self {
        MockMcpTransport {
            tools: Arc::new(tools),
            call_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }
}

#[async_trait]
impl McpTransport for MockMcpTransport {
    async fn list_tools(&self) -> Result<Vec<ToolInfo>> {
        self.call_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(self.tools.as_ref().clone())
    }

    async fn call_tool(&self, name: &str, args: Value) -> Result<Value> {
        let result = serde_json::json!({
            "tool": name,
            "args": args,
            "status": "ok",
        });
        Ok(result)
    }
}

/// MCP over stdio using rmcp (`TokioChildProcess`).
pub struct RmcpChildTransport {
    client: Mutex<RunningService<RoleClient, ()>>,
}

fn handshake_error(server_name: &str, cmd: &str, raw: &str) -> anyhow::Error {
    let lower = raw.to_ascii_lowercase();
    if lower.contains("broken pipe") {
        anyhow!(
            "server '{server_name}': MCP handshake failed.\n\
             The command '{cmd}' exited immediately without responding to MCP initialize.\n\
             Verify the command is a valid MCP server (run it manually to check)."
        )
    } else {
        anyhow!(
            "server '{server_name}': MCP handshake failed (command: '{cmd}').\n\
             Verify the command is a valid MCP server and responds to MCP initialize.\n\
             Details: {raw}"
        )
    }
}

impl RmcpChildTransport {
    pub async fn spawn(server_name: &str, config: &ServerConfig) -> Result<(Self, Option<u32>)> {
        if config.transport != Transport::Stdio {
            return Err(anyhow!(
                "server '{}': only stdio transport is supported for MCP",
                server_name
            ));
        }

        let env_vars = resolve_server_env(config).await?;
        let parts = config.cmd_parts();
        if parts.is_empty() {
            return Err(anyhow!("server '{}' command is empty", server_name));
        }

        let (transport, stderr_opt) =
            TokioChildProcess::builder(tokio::process::Command::new(&parts[0]).configure(|c| {
                c.args(&parts[1..]);
                for (k, v) in &env_vars {
                    c.env(k, v);
                }
            }))
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow!("failed to spawn MCP server '{}': {}", server_name, e))?;

        let pid = transport.id();

        if let Some(mut stderr) = stderr_opt {
            let label = server_name.to_string();
            tokio::spawn(async move {
                let mut reader = BufReader::new(&mut stderr);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line).await {
                        Ok(0) => break,
                        Ok(_) => {
                            let t = line.trim_end();
                            if !t.is_empty() {
                                tracing::debug!(target: "forge_mcp_stderr", server = %label, "{}", t);
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
        }

        let cmd_display = config.cmd.as_deref().unwrap_or("(none)");
        let running = ()
            .serve(transport)
            .await
            .map_err(|e| handshake_error(server_name, cmd_display, &e.to_string()))?;

        Ok((
            Self {
                client: Mutex::new(running),
            },
            pid,
        ))
    }
}

#[async_trait]
impl McpTransport for RmcpChildTransport {
    async fn list_tools(&self) -> Result<Vec<ToolInfo>> {
        let client = self.client.lock().await;
        let tools = client
            .list_all_tools()
            .await
            .map_err(|e| anyhow!("list_tools: {}", e))?;
        Ok(tools
            .into_iter()
            .map(|t| ToolInfo {
                name: t.name.to_string(),
                description: t.description.map(|d| d.to_string()),
                input_schema: Value::Object((*t.input_schema).clone()),
            })
            .collect())
    }

    async fn call_tool(&self, name: &str, args: Value) -> Result<Value> {
        let client = self.client.lock().await;
        let map: JsonObject = match args {
            Value::Object(o) => o,
            Value::Null => JsonObject::new(),
            other => {
                let mut m = JsonObject::new();
                m.insert("value".to_string(), other);
                m
            }
        };
        let params = CallToolRequestParams::new(name.to_string()).with_arguments(map);
        let result = client
            .call_tool(params)
            .await
            .map_err(|e| anyhow!("call_tool: {}", e))?;
        serde_json::to_value(&result).map_err(|e| anyhow!(e))
    }
}

/// MCP over Streamable HTTP (current MCP spec, 2025-03-26+).
pub struct HttpMcpTransport {
    client: Mutex<RunningService<RoleClient, ()>>,
}

impl HttpMcpTransport {
    pub async fn connect_streamable(
        url: &str,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<Self> {
        let config = StreamableHttpClientTransportConfig::with_uri(url).custom_headers(headers);
        let transport = StreamableHttpClientTransport::from_config(config);
        let running: RunningService<RoleClient, _> = ()
            .serve(transport)
            .await
            .map_err(|e| anyhow!("HTTP MCP handshake failed for '{}': {}", url, e))?;
        Ok(Self {
            client: Mutex::new(running),
        })
    }
}

#[async_trait]
impl McpTransport for HttpMcpTransport {
    async fn list_tools(&self) -> Result<Vec<ToolInfo>> {
        let client = self.client.lock().await;
        let tools = client
            .list_all_tools()
            .await
            .map_err(|e| anyhow!("list_tools: {}", e))?;
        Ok(tools
            .into_iter()
            .map(|t| ToolInfo {
                name: t.name.to_string(),
                description: t.description.map(|d| d.to_string()),
                input_schema: Value::Object((*t.input_schema).clone()),
            })
            .collect())
    }

    async fn call_tool(&self, name: &str, args: Value) -> Result<Value> {
        let client = self.client.lock().await;
        let map: JsonObject = match args {
            Value::Object(o) => o,
            Value::Null => JsonObject::new(),
            other => {
                let mut m = JsonObject::new();
                m.insert("value".to_string(), other);
                m
            }
        };
        let params = CallToolRequestParams::new(name.to_string()).with_arguments(map);
        let result = client
            .call_tool(params)
            .await
            .map_err(|e| anyhow!("call_tool: {}", e))?;
        serde_json::to_value(&result).map_err(|e| anyhow!(e))
    }
}

struct LegacySseInner {
    messages_url: String,
    http_client: reqwest::Client,
    pending: Arc<DashMap<u64, oneshot::Sender<Result<serde_json::Value>>>>,
    next_id: AtomicU64,
}

/// MCP over legacy SSE transport (MCP 2024-11-05).
pub struct LegacySseMcpTransport {
    inner: Arc<LegacySseInner>,
}

impl LegacySseMcpTransport {
    pub async fn connect(url: &str, headers: HashMap<HeaderName, HeaderValue>) -> Result<Self> {
        use futures::StreamExt;
        use sse_stream::SseStream;

        let client = reqwest::Client::new();

        let mut req = client.get(url);
        for (k, v) in &headers {
            req = req.header(k.clone(), v.clone());
        }
        req = req.header(reqwest::header::ACCEPT, "text/event-stream");

        let resp = req
            .send()
            .await
            .map_err(|e| anyhow!("SSE connect failed: {}", e))?;
        if !resp.status().is_success() {
            return Err(anyhow!("SSE server returned {}", resp.status()));
        }

        // Box::pin so the stream is heap-allocated and can be moved into
        // tokio::spawn below. The session ID in the endpoint URL is bound to
        // THIS connection — never reconnect after extracting the endpoint event.
        let mut stream = Box::pin(SseStream::from_byte_stream(resp.bytes_stream()));

        let mut messages_url = None;
        while let Some(event) = stream.next().await {
            let event = event.map_err(|e| anyhow!("SSE parse error: {}", e))?;
            if event.event.as_deref() == Some("endpoint") {
                let data = event.data.unwrap_or_default();
                messages_url = Some(if data.starts_with("http") {
                    data
                } else {
                    let base = url.trim_end_matches("/sse").trim_end_matches('/');
                    format!("{}{}", base, data)
                });
                break;
            }
        }

        let messages_url =
            messages_url.ok_or_else(|| anyhow!("SSE server did not send an 'endpoint' event"))?;

        let pending: Arc<DashMap<u64, oneshot::Sender<Result<serde_json::Value>>>> =
            Arc::new(DashMap::new());
        let pending_clone = pending.clone();

        // Move the original stream (not a reconnect) into the background reader.
        // All JSON-RPC responses arrive as `message` events on this same connection.
        tokio::spawn(async move {
            while let Some(Ok(event)) = stream.next().await {
                if event.event.as_deref() == Some("message")
                    && let Some(ref data) = event.data
                    && let Ok(json) = serde_json::from_str::<serde_json::Value>(data)
                    && let Some(id) = json["id"].as_u64()
                    && let Some((_, tx)) = pending_clone.remove(&id)
                {
                    let _ = tx.send(Ok(json));
                }
            }
            // SSE stream ended — wake every waiting caller with an error so they
            // don't block until their per-request timeout fires.
            let keys: Vec<u64> = pending_clone.iter().map(|e| *e.key()).collect();
            for key in keys {
                if let Some((_, tx)) = pending_clone.remove(&key) {
                    let _ = tx.send(Err(anyhow!("SSE connection closed")));
                }
            }
        });

        Ok(Self {
            inner: Arc::new(LegacySseInner {
                messages_url,
                http_client: client,
                pending,
                next_id: AtomicU64::new(1),
            }),
        })
    }

    /// Maximum number of in-flight requests for a single SSE connection.
    const MAX_PENDING: usize = 512;

    async fn send_request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        if self.inner.pending.len() >= Self::MAX_PENDING {
            return Err(anyhow!(
                "SSE pending request limit ({}) reached",
                Self::MAX_PENDING
            ));
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.inner.pending.insert(id, tx);

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let resp = self
            .inner
            .http_client
            .post(&self.inner.messages_url)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                self.inner.pending.remove(&id);
                anyhow!("POST to messages endpoint failed: {}", e)
            })?;

        if !resp.status().is_success() {
            self.inner.pending.remove(&id);
            return Err(anyhow!("messages endpoint returned {}", resp.status()));
        }

        tokio::time::timeout(std::time::Duration::from_secs(30), rx)
            .await
            .map_err(|_| {
                self.inner.pending.remove(&id);
                anyhow!("timeout waiting for SSE response to request {}", id)
            })?
            .map_err(|_| anyhow!("SSE response channel closed unexpectedly"))?
    }
}

#[async_trait]
impl McpTransport for LegacySseMcpTransport {
    async fn list_tools(&self) -> Result<Vec<ToolInfo>> {
        let resp = self
            .send_request("tools/list", serde_json::json!({}))
            .await?;
        if let Some(err) = resp.get("error") {
            return Err(anyhow!("SSE list_tools error: {}", err));
        }
        let tools = resp["result"]["tools"]
            .as_array()
            .ok_or_else(|| anyhow!("SSE list_tools: missing tools array"))?;
        Ok(tools
            .iter()
            .map(|t| ToolInfo {
                name: t["name"].as_str().unwrap_or("").to_string(),
                description: t["description"].as_str().map(|s| s.to_string()),
                input_schema: t
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({"type": "object", "properties": {}})),
            })
            .collect())
    }

    async fn call_tool(&self, name: &str, args: Value) -> Result<Value> {
        let params = serde_json::json!({ "name": name, "arguments": args });
        let resp = self.send_request("tools/call", params).await?;
        if let Some(err) = resp.get("error") {
            return Err(anyhow!("MCP error: {}", err));
        }
        Ok(resp["result"].clone())
    }
}

#[derive(Clone)]
pub struct ToolRegistry {
    transports: Arc<HashMap<String, Arc<dyn McpTransport>>>,
    pids: Arc<DashMap<String, Option<u32>>>,
    cache: PerServerCache,
    ttl: Duration,
    refreshing: RefreshingSet,
}

impl ToolRegistry {
    pub fn new(transports: HashMap<String, Arc<dyn McpTransport>>) -> Self {
        // 60s default: short enough that a restarted server's stale tool list
        // expires quickly. Override with FORGE_TOOL_CACHE_TTL_SECS env var.
        Self::with_options(transports, Duration::from_secs(60))
    }

    pub fn with_options(transports: HashMap<String, Arc<dyn McpTransport>>, ttl: Duration) -> Self {
        let pids = Arc::new(DashMap::new());
        for name in transports.keys() {
            pids.insert(name.clone(), None);
        }
        Self {
            transports: Arc::new(transports),
            pids,
            cache: Arc::new(DashMap::new()),
            ttl,
            refreshing: Arc::new(DashMap::new()),
        }
    }

    pub fn from_build(
        transports: HashMap<String, Arc<dyn McpTransport>>,
        pids: Arc<DashMap<String, Option<u32>>>,
        ttl: Duration,
    ) -> Self {
        Self {
            transports: Arc::new(transports),
            pids,
            cache: Arc::new(DashMap::new()),
            ttl,
            refreshing: Arc::new(DashMap::new()),
        }
    }

    pub fn pids(&self) -> Arc<DashMap<String, Option<u32>>> {
        self.pids.clone()
    }

    pub async fn invalidate_cache(&self) {
        self.cache.clear();
    }

    pub async fn invalidate_server(&self, server: &str) {
        self.cache.remove(server);
    }

    pub async fn list_all_tools(&self) -> Result<Vec<ToolInfo>> {
        let mut tools = Vec::new();
        for (server, transport) in self.transports.iter() {
            let server_tools = self.cached_list_tools(server, transport.as_ref()).await?;
            tools.extend(server_tools.into_iter().map(|mut t| {
                t.name = crate::protocol::namespace_tool(server, &t.name);
                t
            }));
        }
        Ok(tools)
    }

    async fn cached_list_tools(
        &self,
        server: &str,
        transport: &dyn McpTransport,
    ) -> Result<Vec<ToolInfo>> {
        if let Some(entry) = self.cache.get(server) {
            if entry.0.elapsed() < self.ttl {
                // Fresh — return directly.
                return Ok(entry.1.clone());
            }
            // Stale — return stale data immediately (stale-while-revalidate) and
            // kick off a background refresh so the next caller gets fresh data.
            let stale = entry.1.clone();
            drop(entry); // release the DashMap read guard before spawning
            // Only one refresh per server at a time — insert into the set wins the race.
            if self.refreshing.insert(server.to_string(), ()).is_none()
                && let Some(arc_transport) = self.transports.get(server).cloned()
            {
                let cache = self.cache.clone();
                let refreshing = self.refreshing.clone();
                let server_owned = server.to_string();
                tokio::spawn(async move {
                    match arc_transport.list_tools().await {
                        Ok(tools) => {
                            cache.insert(server_owned.clone(), (Instant::now(), tools));
                        }
                        Err(e) => {
                            tracing::warn!(
                                server = %server_owned,
                                "background cache refresh failed: {}",
                                e
                            );
                        }
                    }
                    refreshing.remove(&server_owned);
                });
            }
            return Ok(stale);
        }
        // Not in cache at all — fetch synchronously.
        let tools = transport.list_tools().await?;
        self.cache
            .insert(server.to_string(), (Instant::now(), tools.clone()));
        Ok(tools)
    }

    pub async fn list_tools(&self, server: &str) -> Result<Vec<ToolInfo>> {
        let transport = self
            .transports
            .get(server)
            .ok_or_else(|| anyhow::anyhow!(unknown_server_message(server)))?;
        self.cached_list_tools(server, transport.as_ref()).await
    }

    pub async fn call_tool(&self, namespaced_tool: &str, args: Value) -> Result<Value> {
        let (server, tool) = crate::protocol::parse_namespaced_tool(namespaced_tool)
            .ok_or_else(|| anyhow!("invalid tool name: {}", namespaced_tool))?;

        let transport = self
            .transports
            .get(server)
            .ok_or_else(|| anyhow::anyhow!(unknown_server_message(server)))?;

        transport.call_tool(tool, args).await
    }
}

/// Message prefix for an unregistered server name (used by proxy error mapping).
pub fn unknown_server_message(server: &str) -> String {
    format!("unknown server: {server}")
}

/// Returns true when `err` refers to an unregistered server name.
pub fn is_unknown_server_error(err: &anyhow::Error) -> bool {
    err.to_string().starts_with("unknown server:")
}

/// Read tool-cache TTL from `FORGE_TOOL_CACHE_TTL_SECS`, default 60s.
/// Logs a warning when the env var is set but not a valid positive integer.
pub fn tool_cache_ttl_from_env() -> Duration {
    const DEFAULT_SECS: u64 = 60;
    match std::env::var("FORGE_TOOL_CACHE_TTL_SECS") {
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(0) => {
                warn!(
                    value = %raw,
                    "FORGE_TOOL_CACHE_TTL_SECS must be > 0, using default {DEFAULT_SECS}s"
                );
                Duration::from_secs(DEFAULT_SECS)
            }
            Ok(secs) => Duration::from_secs(secs),
            Err(_) => {
                warn!(
                    value = %raw,
                    "invalid FORGE_TOOL_CACHE_TTL_SECS, using default {DEFAULT_SECS}s"
                );
                Duration::from_secs(DEFAULT_SECS)
            }
        },
        Err(_) => Duration::from_secs(DEFAULT_SECS),
    }
}

async fn build_auth_headers(
    server_name: &str,
    config: &ServerConfig,
) -> Result<HashMap<HeaderName, HeaderValue>> {
    let resolver = DefaultSecretResolver;
    let mut map = HashMap::new();
    for (header_name, secret_ref) in &config.secret {
        let value = resolver
            .resolve(server_name, secret_ref)
            .await
            .map_err(|e| {
                anyhow!(
                    "failed to resolve secret for header '{}': {}",
                    header_name,
                    e
                )
            })?;
        let name = HeaderName::from_bytes(header_name.as_bytes())
            .map_err(|e| anyhow!("invalid header name '{}': {}", header_name, e))?;
        let val = HeaderValue::from_str(value.expose_secret())
            .map_err(|e| anyhow!("invalid header value for '{}': {}", header_name, e))?;
        map.insert(name, val);
    }
    Ok(map)
}

/// Default timeout for remote MCP reachability probes (`forge check`, etc.).
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Verify an HTTP or SSE MCP server accepts a connection and responds to `tools/list`.
pub async fn probe_server_reachable(name: &str, config: &ServerConfig) -> Result<()> {
    use crate::config::Transport;

    let url = config
        .url
        .as_deref()
        .filter(|u| !u.trim().is_empty())
        .ok_or_else(|| anyhow!("server '{name}': url required for remote transport"))?;

    let headers = build_auth_headers(name, config).await?;
    let probe = async {
        match config.transport {
            Transport::Http => {
                let transport = HttpMcpTransport::connect_streamable(url, headers).await?;
                transport.list_tools().await?;
            }
            Transport::Sse => {
                let transport = LegacySseMcpTransport::connect(url, headers).await?;
                transport.list_tools().await?;
            }
            Transport::Stdio => {}
        }
        Ok::<(), anyhow::Error>(())
    };

    tokio::time::timeout(PROBE_TIMEOUT, probe)
        .await
        .map_err(|_| {
            anyhow!(
                "server '{name}': timed out after {}s",
                PROBE_TIMEOUT.as_secs()
            )
        })?
}

/// Connect all configured MCP servers (stdio, HTTP, or legacy SSE).
pub async fn build_tool_registry(config: &ForgeConfig) -> Result<ToolRegistry> {
    let ttl = tool_cache_ttl_from_env();

    let mut map: HashMap<String, Arc<dyn McpTransport>> = HashMap::new();
    let pids: Arc<DashMap<String, Option<u32>>> = Arc::new(DashMap::new());

    for (name, server_cfg) in &config.server {
        match server_cfg.transport {
            Transport::Stdio => {
                let (t, pid) = RmcpChildTransport::spawn(name, server_cfg).await?;
                map.insert(name.clone(), Arc::new(t));
                pids.insert(name.clone(), pid);
            }
            Transport::Http => {
                let url = server_cfg
                    .url
                    .as_deref()
                    .ok_or_else(|| anyhow!("server '{}': url required for http transport", name))?;
                let headers = build_auth_headers(name, server_cfg).await?;
                let transport = HttpMcpTransport::connect_streamable(url, headers)
                    .await
                    .map_err(|e| anyhow!("server '{}': {}", name, e))?;
                map.insert(name.clone(), Arc::new(transport));
                pids.insert(name.clone(), None);
            }
            Transport::Sse => {
                let url = server_cfg
                    .url
                    .as_deref()
                    .ok_or_else(|| anyhow!("server '{}': url required for sse transport", name))?;
                let headers = build_auth_headers(name, server_cfg).await?;
                let transport = LegacySseMcpTransport::connect(url, headers)
                    .await
                    .map_err(|e| anyhow!("server '{}': {}", name, e))?;
                map.insert(name.clone(), Arc::new(transport));
                pids.insert(name.clone(), None);
            }
        }
    }

    if map.is_empty() {
        return Err(anyhow!("no MCP servers could be connected"));
    }

    Ok(ToolRegistry::from_build(map, pids, ttl))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::Arc;

    #[test]
    fn test_is_unknown_server_error() {
        let err = anyhow::anyhow!(unknown_server_message("missing"));
        assert!(is_unknown_server_error(&err));
        assert!(!is_unknown_server_error(&anyhow::anyhow!("other failure")));
    }

    #[test]
    fn handshake_error_broken_pipe_gives_actionable_message() {
        let err = handshake_error(
            "myserver",
            "echo",
            "Send message error Transport [rmcp::transport::child_process::TokioChildProcess] \
             error: Broken pipe (os error 32), when send initialize request",
        );
        let msg = err.to_string();
        assert!(msg.contains("server 'myserver'"));
        assert!(msg.contains("'echo'"));
        assert!(msg.contains("exited immediately"));
        assert!(msg.contains("valid MCP server"));
        assert!(!msg.contains("rmcp::"));
    }

    #[test]
    fn handshake_error_other_error_includes_details() {
        let err = handshake_error("srv", "my-cmd", "connection refused");
        let msg = err.to_string();
        assert!(msg.contains("server 'srv'"));
        assert!(msg.contains("'my-cmd'"));
        assert!(msg.contains("connection refused"));
    }

    #[test]
    fn test_tool_cache_ttl_from_env_invalid_falls_back_to_60() {
        // SAFETY: test-only env mutation.
        unsafe { std::env::set_var("FORGE_TOOL_CACHE_TTL_SECS", "not_a_number") };
        assert_eq!(tool_cache_ttl_from_env(), Duration::from_secs(60));
        unsafe { std::env::remove_var("FORGE_TOOL_CACHE_TTL_SECS") };
    }

    #[test]
    fn test_tool_cache_ttl_from_env_zero_falls_back_to_60() {
        unsafe { std::env::set_var("FORGE_TOOL_CACHE_TTL_SECS", "0") };
        assert_eq!(tool_cache_ttl_from_env(), Duration::from_secs(60));
        unsafe { std::env::remove_var("FORGE_TOOL_CACHE_TTL_SECS") };
    }

    #[test]
    fn test_tool_cache_ttl_from_env_valid_value() {
        unsafe { std::env::set_var("FORGE_TOOL_CACHE_TTL_SECS", "120") };
        assert_eq!(tool_cache_ttl_from_env(), Duration::from_secs(120));
        unsafe { std::env::remove_var("FORGE_TOOL_CACHE_TTL_SECS") };
    }

    #[tokio::test]
    async fn test_probe_server_reachable_fails_on_unreachable_http() {
        let cfg = ForgeConfig::parse_str(
            r#"
[server.dead]
transport = "http"
url = "http://127.0.0.1:1"
"#,
        )
        .unwrap();
        let server = &cfg.server["dead"];
        let result = probe_server_reachable("dead", server).await;
        assert!(result.is_err(), "probe should fail for unreachable server");
    }

    #[tokio::test]
    async fn test_tool_registry_default_ttl_is_60s() {
        let transports: HashMap<String, Arc<dyn McpTransport>> = HashMap::new();
        let registry = ToolRegistry::new(transports);
        assert_eq!(registry.ttl, Duration::from_secs(60));
    }

    #[tokio::test]
    async fn test_mock_transport_new_returns_empty_schemas() {
        let transport = MockMcpTransport::new(vec!["echo".to_string()]);
        let tools = transport.list_tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        assert!(tools[0].description.is_none());
        assert_eq!(
            tools[0].input_schema,
            serde_json::json!({"type": "object", "properties": {}})
        );
    }

    #[tokio::test]
    async fn test_mock_transport_with_schemas_returns_provided_schema() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "message": { "type": "string" } },
            "required": ["message"]
        });
        let tools = vec![ToolInfo {
            name: "echo".to_string(),
            description: Some("Echo a message".to_string()),
            input_schema: schema.clone(),
        }];
        let transport = MockMcpTransport::with_schemas(tools);
        let result = transport.list_tools().await.unwrap();
        assert_eq!(result[0].description.as_deref(), Some("Echo a message"));
        assert_eq!(result[0].input_schema, schema);
    }

    #[tokio::test]
    async fn test_tool_registry_list_all_tools_returns_full_tool_info() {
        let schema = serde_json::json!({"type": "object", "properties": {}});
        let tools = vec![ToolInfo {
            name: "build".to_string(),
            description: Some("Build the project".to_string()),
            input_schema: schema.clone(),
        }];
        let transport = MockMcpTransport::with_schemas(tools);
        let mut transports: HashMap<String, Arc<dyn McpTransport>> = HashMap::new();
        transports.insert("ci".to_string(), Arc::new(transport));
        let registry = ToolRegistry::new(transports);

        let tools = registry.list_all_tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "ci__build");
        assert_eq!(tools[0].description.as_deref(), Some("Build the project"));
    }

    #[tokio::test]
    async fn test_http_mcp_transport_connect_streamable_fails_on_invalid_url() {
        let headers = HashMap::new();
        let result =
            HttpMcpTransport::connect_streamable("http://127.0.0.1:19999/nonexistent", headers)
                .await;
        assert!(
            result.is_err(),
            "connecting to non-existent server should fail"
        );
    }

    #[tokio::test]
    async fn test_legacy_sse_transport_connect_fails_on_invalid_url() {
        let headers = HashMap::new();
        let result = LegacySseMcpTransport::connect("http://127.0.0.1:19998/sse", headers).await;
        assert!(
            result.is_err(),
            "connection to non-existent SSE server should fail"
        );
    }

    #[tokio::test]
    async fn test_build_tool_registry_http_without_url_errors() {
        let cfg = r#"
[server.broken-http]
transport = "http"
url = ""
"#;
        let result = ForgeConfig::parse_str(cfg);
        assert!(
            result.is_err(),
            "http server with empty url should fail config validation"
        );
    }

    #[tokio::test]
    async fn list_all_tools_namespaces_tools() {
        let mut transports: HashMap<String, Arc<dyn McpTransport>> = HashMap::new();
        transports.insert(
            "local".to_string(),
            Arc::new(MockMcpTransport::new(vec![
                "build".to_string(),
                "test".to_string(),
            ])),
        );

        let registry = ToolRegistry::new(transports);
        let tools = registry.list_all_tools().await.unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"local__build"));
        assert!(names.contains(&"local__test"));
    }

    #[tokio::test]
    async fn call_tool_routes_namespaced_tool() {
        let mut transports: HashMap<String, Arc<dyn McpTransport>> = HashMap::new();
        transports.insert(
            "local".to_string(),
            Arc::new(MockMcpTransport::new(vec!["build".to_string()])),
        );

        let registry = ToolRegistry::new(transports);
        let result = registry
            .call_tool("local__build", json!({ "task": "compile" }))
            .await
            .unwrap();

        assert_eq!(result["tool"], "build");
        assert_eq!(result["args"]["task"], "compile");
    }

    #[tokio::test]
    async fn stale_cache_returns_immediately_and_triggers_background_refresh() {
        let transport = Arc::new(MockMcpTransport::new(vec!["build".to_string()]));
        let call_count = transport.call_count.clone();

        let mut transports: HashMap<String, Arc<dyn McpTransport>> = HashMap::new();
        transports.insert("local".to_string(), transport);

        // Create registry with a 1ms TTL so the cache expires immediately.
        let registry = ToolRegistry::with_options(transports, Duration::from_millis(1));

        // Prime the cache (1st transport call).
        let first = registry.list_tools("local").await.unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].name, "build");
        assert_eq!(
            call_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "expected one transport call to prime the cache"
        );

        // Wait for TTL to elapse.
        tokio::time::sleep(Duration::from_millis(5)).await;

        // Second call on a stale cache should still return the stale data without
        // blocking (stale-while-revalidate). The background refresh runs concurrently.
        let second = registry.list_tools("local").await.unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].name, "build");

        // Poll for the background refresh to complete (bounded timeout avoids
        // flakiness on slow CI while not blocking indefinitely).
        let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
        loop {
            if call_count.load(std::sync::atomic::Ordering::SeqCst) >= 2 {
                break;
            }
            let now = tokio::time::Instant::now();
            assert!(now < deadline, "timed out waiting for background refresh");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // Verify the background refresh actually called the transport a second time.
        assert_eq!(
            call_count.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "expected exactly 2 transport calls (1 initial + 1 background refresh)"
        );
        assert!(!registry.cache.is_empty());
    }

    /// A transport whose list_tools always returns an Err, used to verify that
    /// the ToolRegistry propagates transport errors rather than silently swallowing them.
    struct AlwaysErrTransport;

    #[async_trait::async_trait]
    impl McpTransport for AlwaysErrTransport {
        async fn list_tools(&self) -> Result<Vec<ToolInfo>> {
            Err(anyhow!(
                "SSE list_tools error: {{\"code\":-32601,\"message\":\"Method not found\"}}"
            ))
        }
        async fn call_tool(&self, _name: &str, _args: Value) -> Result<Value> {
            Err(anyhow!("not implemented"))
        }
    }

    #[tokio::test]
    async fn list_tools_propagates_transport_error() {
        let mut transports: HashMap<String, Arc<dyn McpTransport>> = HashMap::new();
        transports.insert("bad".to_string(), Arc::new(AlwaysErrTransport));

        let registry = ToolRegistry::new(transports);
        let result = registry.list_tools("bad").await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("SSE list_tools error"),
            "expected error message, got: {msg}"
        );
    }
}
