# mcp-forge v1.0 Enhancement Design

**Date:** 2026-06-21  
**Branch:** development  
**Status:** Approved  

---

## 1. Problem Statement

mcp-forge v0.1.1 is a working MVS: it proxies stdio MCP servers, enforces rate limits and RBAC, and logs everything to SQLite. But three gaps block v1.0 and limit virality:

1. **Tool schemas are empty.** `tools/list` returns `inputSchema: {}` for every tool. Agent frameworks that validate arguments before calling fail silently or incorrectly.
2. **Only local stdio servers are supported.** Every major hosted MCP provider (GitHub, Linear, Notion, Anthropic) operates over HTTP. forge cannot connect to them.
3. **The proxy is unauthenticated.** Any local process can call it. Once forge proxies paid remote APIs, this is unacceptable.

Additionally, the validation report (2026-03-31) identified 2 critical and 5 medium bugs that undermine correctness and operator trust.

---

## 2. Goals

- Fix all critical/high bugs and the most dangerous medium issues before adding features.
- Pass real tool schemas (name, description, inputSchema) through the proxy to agents.
- Connect to remote MCP servers via Streamable HTTP and legacy SSE — same guardrails as stdio.
- Protect the proxy with an optional Bearer token.
- Ship as v1.0.0 on the existing release pipeline.

---

## 3. Non-Goals

- `forge-transport` as a separate crate — YAGNI until v2.0.
- 80% blanket test coverage — targeted coverage of risky paths only.
- mTLS, OAuth, or JWT auth on the proxy — Bearer token is sufficient for v1.0.
- Windows named-pipe daemon IPC — foreground mode only for v1.0.
- WebSocket transport — not in the MCP spec.

---

## 4. Architecture

No new crates. The four-crate workspace stays intact.

```
forge-cli        forge-proxy          forge-core
   │                  │                   │
   │            ProxyAppState             │
   │            ├── ToolRegistry ─────────┤
   │            ├── AuditWriter           │
   │            ├── RateLimiters          │
   │            ├── CostGuard             │
   │            ├── RbacPolicies          │
   │            ├── InjectionDetector     │
   │            └── AuthMiddleware (new)  │
   │                                      │
   │                              McpTransport (trait)
   │                              ├── RmcpChildTransport (stdio, existing)
   │                              └── HttpMcpTransport (new)
   │                                  ├── StreamableHttp (rmcp feature)
   │                                  └── Sse (rmcp feature)
   │
forge-mock-mcp
   └── --http mode (new, for E2E tests)
```

Data flow is unchanged: every tool call, regardless of transport, passes through the same RBAC → rate-limit → cost-guard → injection-scan → audit pipeline in `forge-proxy`.

---

## 5. Phase 1 — Hardening

**Deliverable:** `cargo test --all` green, `cargo clippy -- -D warnings` clean. All items below fixed.

### 5.1 Bug Fixes

| ID | Severity | File | Fix |
|----|----------|------|-----|
| C1 | Critical | `commands/check.rs` | Gate glob success message on `allow_checked > 0 && errors == 0`. Dead variables removed. |
| M2 | Medium→High | `proxy/lib.rs` | Verify injection `Block` mode returns `-32002` to caller. Currently config-driven but untested end-to-end. |
| H3 | High | `commands/logs.rs` | Replace `read_to_string` in `--follow` loop with `BufReader::lines()`. Eliminates unbounded allocation. |
| M4 | Medium | `commands/init.rs`, `add.rs` | Validate server names: reject `/`, `..`, whitespace, shell metacharacters. Use a `validate_server_name(s: &str) -> Result<()>` function shared between both commands. |
| M5 | Medium | `core/audit.rs` | Add `schema_version` table to SQLite. `AuditWriter::new()` runs migration on open: checks current version, applies missing migrations sequentially. |
| M6 | Medium | `core/supervisor/mod.rs` | Call `registry.invalidate_server(name)` at the top of each restart loop iteration, before respawning the child. |

### 5.2 Targeted Tests

Four test additions, all in existing test files:

**RBAC deny path** (`forge-proxy/tests/security_hardening.rs`):
- Configure a server with `deny_tools = ["delete_*"]`
- Call `delete_repo` via proxy
- Assert response error code is `-32001` and body contains `"policy"`

**Injection block path** (`forge-proxy/tests/security_hardening.rs`):
- Set `guard.injection_mode = "block"`
- Call a tool with args containing `"ignore all previous instructions"`
- Assert response error code is `-32002`

**Cost guard boundary** (`forge-proxy/src/lib.rs` `#[cfg(test)]`):
- Already has concurrent test — extend to verify exactly-at-limit behaviour and day rollover using a mocked clock via `FORGE_TEST_DAY_KEY` env var

**Proxy E2E round-trip** (`forge-proxy/tests/`):
- `MockMcpTransport` with two tools
- Build `ProxyAppState`, spin up `build_router`
- `tools/list` → assert two namespaced tools returned with correct names
- `tools/call` → assert result routed correctly
- Rate-limit exhaustion → assert `-32000` returned

---

## 6. Phase 2 — Schema Passthrough

**Deliverable:** `tools/list` returns real `name`, `description`, and `inputSchema` from every upstream server.

### 6.1 `ToolInfo` type (forge-core)

```rust
// forge_core::mcp
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInfo {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: serde_json::Value,
}
```

### 6.2 `McpTransport` trait update

```rust
#[async_trait]
pub trait McpTransport: Send + Sync {
    async fn list_tools(&self) -> Result<Vec<ToolInfo>>;   // was Vec<String>
    async fn call_tool(&self, name: &str, args: Value) -> Result<Value>;
}
```

### 6.3 `RmcpChildTransport` update

rmcp's `list_all_tools()` returns `Vec<Tool>` where `Tool` has `name`, `description`, and `input_schema`. Map directly:

```rust
async fn list_tools(&self) -> Result<Vec<ToolInfo>> {
    let client = self.client.lock().await;
    let tools = client.list_all_tools().await?;
    Ok(tools.into_iter().map(|t| ToolInfo {
        name: t.name.to_string(),
        description: t.description.map(|d| d.to_string()),
        input_schema: serde_json::to_value(&t.input_schema).unwrap_or_else(|_| json!({"type":"object","properties":{}})),
    }).collect())
}
```

### 6.4 `MockMcpTransport` update

Keeps the `new(vec!["build", "test"])` convenience constructor (defaults to empty schemas). Adds:

```rust
pub fn with_schemas(tools: Vec<ToolInfo>) -> Self { ... }
```

### 6.5 `ToolRegistry` cache update

`PerServerCache` changes from `Vec<String>` to `Vec<ToolInfo>`. `list_all_tools` prefixes `tool.name` with `server__` and returns full structs. `call_tool` route logic unchanged (still splits on `__`).

### 6.6 Proxy response update

`handle_tools_list` serialises full schema:

```json
{
  "tools": [{
    "name": "github__create_issue",
    "description": "Create a GitHub issue",
    "inputSchema": { "type": "object", "properties": { "title": {"type":"string"} }, "required": ["title"] }
  }]
}
```

---

## 7. Phase 3 — HTTP Transport

**Deliverable:** Remote MCP servers reachable via `transport = "http"` (Streamable HTTP) and `transport = "sse"` (legacy SSE). All forge guardrails apply identically.

### 7.1 Config

```toml
[server.github-remote]
transport = "http"
url = "https://api.github.com/mcp"
secret.Authorization = "env:GH_TOKEN"

[server.linear-remote]
transport = "sse"
url = "https://mcp.linear.app/sse"
secret.Authorization = "env:LINEAR_TOKEN"
max_calls_per_min = 30
```

`ServerConfig` gains `url: Option<String>`. Validation in `forge check` requires `url` when `transport` is `http` or `sse`, requires `cmd` when `transport` is `stdio`.

### 7.2 `Transport` enum

```rust
pub enum Transport {
    Stdio,
    Http,   // Streamable HTTP (current MCP spec)
    Sse,    // Legacy SSE (MCP 2024-11-05, still used by Cursor etc.)
}
```

Config TOML values: `"stdio"` | `"http"` | `"sse"`.

### 7.3 rmcp features

```toml
# workspace Cargo.toml
rmcp = { version = "1.3", default-features = false, features = [
    "client",
    "transport-child-process",
    "transport-streamable-http-client-reqwest",   # Streamable HTTP
    "client-side-sse",                            # Legacy SSE
] }
```

### 7.4 `HttpMcpTransport`

New struct in `forge_core::mcp`. Implements `McpTransport` identically to `RmcpChildTransport`. Secrets declared in `[server.x].secret` are injected as HTTP headers on every outbound request (not env vars):

```rust
pub struct HttpMcpTransport {
    client: Mutex<RunningService<RoleClient, ()>>,
}

impl HttpMcpTransport {
    pub async fn connect_streamable(url: &str, headers: HeaderMap) -> Result<Self> {
        let transport = rmcp::transport::streamable_http_client::StreamableHttpClientTransport::new(
            url, headers
        );
        let running = ().serve(transport).await?;
        Ok(Self { client: Mutex::new(running) })
    }

    pub async fn connect_sse(url: &str, headers: HeaderMap) -> Result<Self> {
        let transport = rmcp::transport::sse_client::SseClientTransport::new(url, headers);
        let running = ().serve(transport).await?;
        Ok(Self { client: Mutex::new(running) })
    }
}
```

`McpTransport` impl is identical to `RmcpChildTransport` — same `list_tools` + `call_tool` body.

### 7.5 `build_tool_registry` dispatch

```rust
Transport::Http => {
    let headers = build_auth_headers(server_cfg).await?;
    let t = HttpMcpTransport::connect_streamable(
        server_cfg.url.as_deref().ok_or_else(|| anyhow!("server '{}': url required for http transport", name))?,
        headers,
    ).await?;
    map.insert(name.clone(), Arc::new(t));
    pids.insert(name.clone(), None);  // no pid for remote servers
}
Transport::Sse => { /* same, connect_sse */ }
```

### 7.6 `forge status` for HTTP servers

`ServerHealth` gains `HttpConnected { url: String }` and `HttpDisconnected { url: String, error: String }` variants. The status table renders these without a PID column.

### 7.7 `forge check` addition

For HTTP/SSE servers: send an MCP `initialize` request to the URL. If it returns a valid response, print `[OK] reachable`. If it times out or errors, print `[ERR] unreachable: <reason>`.

### 7.8 `forge-mock-mcp` HTTP mode

```bash
forge-mock-mcp --http 0.0.0.0:9999 --tools 3
```

Starts a Streamable HTTP MCP server on the given port. Used in E2E integration tests — eliminates the need for real remote services in CI.

---

## 8. Phase 4 — Proxy Auth

**Deliverable:** Optional Bearer token on the proxy HTTP listener. Exempt: `/.well-known/`.

### 8.1 Config

```toml
[proxy]
bind = "127.0.0.1"
port = 3456
auth_token = "env:FORGE_AUTH_TOKEN"   # if absent: no auth enforced
```

`ProxyConfig` gains `auth_token: Option<SecretRef>`. Resolved at startup into a `SecretString`, stored in `ProxyAppState`.

### 8.2 Tower middleware

```rust
pub struct AuthLayer {
    token: Option<SecretString>,  // None = disabled
}
```

Applied at router level, before all handlers except `/.well-known/`. On every request:
1. If `token` is `None` → pass through
2. Extract `Authorization` header. If missing → `401 Unauthorized`
3. Strip `"Bearer "` prefix. Compare with stored token using constant-time equality (`subtle` crate, already transitively available)
4. Mismatch → `401 Unauthorized`
5. Match → pass through

### 8.3 Startup warning

If `proxy.auth_token` is not set and `proxy.bind` is not `127.0.0.1` or `::1`, `forge start` prints:

```
warning: proxy is listening on a non-loopback address without auth_token set.
         Any process on the network can call your MCP tools.
         Set [proxy] auth_token in forge.toml to require authentication.
```

---

## 9. Data Flow Summary

```
Agent
  │ POST / (Bearer token validated by AuthLayer)
  ▼
forge-proxy: dispatch_request
  ├── InjectionDetector.scan_arguments()
  ├── RbacPolicy.is_allowed(tool)
  ├── RateLimiter.check(server)
  ├── CostGuard.check(server, max_per_day)
  │
  ├── ToolRegistry.call_tool(namespaced_tool, args)
  │     ├── RmcpChildTransport  (stdio servers)
  │     └── HttpMcpTransport    (http/sse servers)  ← new
  │
  ├── InjectionDetector.scan_result()
  └── AuditWriter.log(event)
```

---

## 10. Testing Strategy

**Phase 1 tests:** Four targeted additions (see §5.2).

**Phase 2 tests:** Update existing `tools_list_returns_namespaced_tools` proxy test to assert `inputSchema` is non-empty when `MockMcpTransport::with_schemas()` is used.

**Phase 3 tests:**
- Unit: `HttpMcpTransport::connect_*` against `forge-mock-mcp --http` in the same process
- Integration: full proxy E2E with an HTTP backend using `forge-mock-mcp`
- `forge check` HTTP reachability test using `forge-mock-mcp --http`

**Phase 4 tests:**
- Auth middleware: request without token → 401
- Auth middleware: request with wrong token → 401
- Auth middleware: correct token → 200
- `.well-known` exempt from auth

---

## 11. Dependency Changes

| Package | Change | Reason |
|---------|--------|--------|
| `rmcp` features | Add `transport-streamable-http-client-reqwest`, `client-side-sse` | HTTP transport |
| `subtle` | Add (check if already transitive; if not, add directly) | Constant-time token comparison |

No new crates required beyond rmcp feature flags and `subtle`.

---

## 12. Release

Phase 1 ships as a patch (`v0.1.2`). Phases 2-4 ship together as `v1.0.0` — they form a coherent feature story: "real schemas, remote servers, auth."

v1.0.0 checklist update:
- [ ] `tools/list` returns real schemas for all server types
- [ ] `http` and `sse` transport configured and working
- [ ] Bearer token auth validated end-to-end
- [ ] All hardening bugs fixed
- [ ] Targeted test suite green
- [ ] cross-platform artifacts available
- [ ] installer and Homebrew validated
- [ ] CHANGELOG prepared
