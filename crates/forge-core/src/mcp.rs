use anyhow::{Result, anyhow};
use async_trait::async_trait;
use dashmap::DashMap;
use rmcp::{
    RoleClient, ServiceExt,
    model::{CallToolRequestParams, JsonObject},
    service::RunningService,
    transport::{ConfigureCommandExt, TokioChildProcess},
};
use serde_json::Value;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::Mutex;
use tracing::warn;

/// Per-server tool list cache: server name → (cached_at, tool names).
type PerServerCache = Arc<DashMap<String, (Instant, Vec<String>)>>;

/// Set of server names currently undergoing a background cache refresh.
/// Presence in this map means a refresh task is already in-flight.
type RefreshingSet = Arc<DashMap<String, ()>>;

use crate::config::{ForgeConfig, ServerConfig, Transport, resolve_server_env};

#[async_trait]
pub trait McpTransport: Send + Sync {
    async fn list_tools(&self) -> Result<Vec<String>>;
    async fn call_tool(&self, name: &str, args: Value) -> Result<Value>;
}

#[derive(Debug, Clone)]
pub struct MockMcpTransport {
    pub tools: Arc<Vec<String>>,
    pub call_count: Arc<std::sync::atomic::AtomicUsize>,
}

impl MockMcpTransport {
    pub fn new<T: Into<Vec<String>>>(tools: T) -> Self {
        MockMcpTransport {
            tools: Arc::new(tools.into()),
            call_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }
}

#[async_trait]
impl McpTransport for MockMcpTransport {
    async fn list_tools(&self) -> Result<Vec<String>> {
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

        let running = ()
            .serve(transport)
            .await
            .map_err(|e| anyhow!("MCP handshake failed for '{}': {}", server_name, e))?;

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
    async fn list_tools(&self) -> Result<Vec<String>> {
        let client = self.client.lock().await;
        let tools = client
            .list_all_tools()
            .await
            .map_err(|e| anyhow!("list_tools: {}", e))?;
        Ok(tools.into_iter().map(|t| t.name.to_string()).collect())
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
        Self::with_options(transports, Duration::from_secs(300))
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

    pub async fn list_all_tools(&self) -> Result<Vec<String>> {
        let mut tools = Vec::new();
        for (server, transport) in self.transports.iter() {
            let server_tools = self.cached_list_tools(server, transport.as_ref()).await?;
            tools.extend(
                server_tools
                    .into_iter()
                    .map(|tool| crate::protocol::namespace_tool(server, &tool)),
            );
        }
        Ok(tools)
    }

    async fn cached_list_tools(
        &self,
        server: &str,
        transport: &dyn McpTransport,
    ) -> Result<Vec<String>> {
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
            if self.refreshing.insert(server.to_string(), ()).is_none() {
                if let Some(arc_transport) = self.transports.get(server).cloned() {
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
            }
            return Ok(stale);
        }
        // Not in cache at all — fetch synchronously.
        let tools = transport.list_tools().await?;
        self.cache
            .insert(server.to_string(), (Instant::now(), tools.clone()));
        Ok(tools)
    }

    pub async fn list_tools(&self, server: &str) -> Result<Vec<String>> {
        let transport = self
            .transports
            .get(server)
            .ok_or_else(|| anyhow!("unknown server: {}", server))?;
        self.cached_list_tools(server, transport.as_ref()).await
    }

    pub async fn call_tool(&self, namespaced_tool: &str, args: Value) -> Result<Value> {
        let (server, tool) = crate::protocol::parse_namespaced_tool(namespaced_tool)
            .ok_or_else(|| anyhow!("invalid tool name: {}", namespaced_tool))?;

        let transport = self
            .transports
            .get(server)
            .ok_or_else(|| anyhow!("unknown server: {}", server))?;

        transport.call_tool(tool, args).await
    }
}

/// Connect all configured stdio servers via rmcp.
pub async fn build_tool_registry(config: &ForgeConfig) -> Result<ToolRegistry> {
    let ttl_secs = std::env::var("FORGE_TOOL_CACHE_TTL_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(300);
    let ttl = Duration::from_secs(ttl_secs);

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
                warn!(
                    "server '{}': http transport skipped (not implemented)",
                    name
                );
            }
        }
    }

    if map.is_empty() {
        return Err(anyhow!(
            "no stdio MCP servers configured (http transport is not supported yet)"
        ));
    }

    Ok(ToolRegistry::from_build(map, pids, ttl))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::Arc;

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

        assert_eq!(tools, vec!["local__build", "local__test"]);
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
        assert_eq!(first, vec!["build"]);
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
        assert_eq!(second, vec!["build"]);

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
}
