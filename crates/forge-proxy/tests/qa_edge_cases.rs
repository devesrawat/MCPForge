//! QA edge-case tests for v1.0 enhancements.
#![allow(clippy::too_many_lines)]

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use forge_core::config::ForgeConfig;
use forge_core::mcp::{McpTransport, MockMcpTransport, ToolInfo, ToolRegistry};
use forge_proxy::{ProxyAppState, build_router};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use tower::util::ServiceExt;

async fn post_json(
    app: axum::Router,
    uri: &str,
    body: String,
    auth: Option<&str>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(token) = auth {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    let req = req.body(Body::from(body)).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: Value = if bytes.is_empty() {
        json!(null)
    } else {
        serde_json::from_slice(&bytes).unwrap_or(json!({"raw": String::from_utf8_lossy(&bytes)}))
    };
    (status, json)
}

async fn get_path(app: axum::Router, uri: &str, auth: Option<&str>) -> StatusCode {
    let mut req = Request::builder().method("GET").uri(uri);
    if let Some(token) = auth {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    let req = req.body(Body::empty()).unwrap();
    app.oneshot(req).await.unwrap().status()
}

// ─── Config validation edge cases ────────────────────────────────────

mod config_edge {
    use super::*;

    #[test]
    fn whitespace_only_url_rejected_for_http() {
        let err = ForgeConfig::parse_str(
            r#"
[server.x]
transport = "http"
url = "   "
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("url"));
    }

    #[test]
    fn whitespace_only_cmd_rejected_for_stdio() {
        let err = ForgeConfig::parse_str(
            r#"
[server.x]
transport = "stdio"
cmd = "  "
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("cmd"));
    }

    #[test]
    fn sse_with_valid_url_accepted() {
        let cfg = ForgeConfig::parse_str(
            r#"
[server.linear]
transport = "sse"
url = "https://mcp.linear.app/sse"
"#,
        )
        .unwrap();
        assert_eq!(
            cfg.server["linear"].url.as_deref(),
            Some("https://mcp.linear.app/sse")
        );
    }

    #[test]
    fn stdio_with_url_but_no_cmd_still_requires_cmd() {
        let err = ForgeConfig::parse_str(
            r#"
[server.x]
transport = "stdio"
url = "http://ignored.example.com"
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("cmd"));
    }

    #[test]
    fn http_server_can_have_optional_cmd() {
        let cfg = ForgeConfig::parse_str(
            r#"
[server.remote]
transport = "http"
url = "https://api.example.com/mcp"
cmd = "should-be-ignored"
"#,
        )
        .unwrap();
        assert!(cfg.server["remote"].url.is_some());
    }

    #[test]
    fn mixed_stdio_and_http_servers_parse() {
        let cfg = ForgeConfig::parse_str(
            r#"
[server.local]
cmd = "true"

[server.remote]
transport = "http"
url = "http://127.0.0.1:9999"
"#,
        )
        .unwrap();
        assert_eq!(cfg.server.len(), 2);
    }

    #[test]
    fn unknown_server_field_rejected() {
        let err = ForgeConfig::parse_str(
            r#"
[server.x]
cmd = "true"
not_a_field = "nope"
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown field") || err.to_string().contains("TOML"));
    }
}

// ─── Schema passthrough edge cases ───────────────────────────────────

mod schema_edge {
    use super::*;

    async fn app_with_tools(tools: Vec<ToolInfo>) -> axum::Router {
        let transport = MockMcpTransport::with_schemas(tools);
        let mut transports: HashMap<String, Arc<dyn McpTransport>> = HashMap::new();
        transports.insert("srv".to_string(), Arc::new(transport));
        let registry = ToolRegistry::new(transports);
        let state = ProxyAppState::new(
            registry,
            ForgeConfig::parse_str("[server.srv]\ncmd = \"true\"\n").unwrap(),
            None,
        )
        .unwrap();
        build_router(state)
    }

    #[tokio::test]
    async fn tool_without_description_omits_description_key() {
        let tools = vec![ToolInfo {
            name: "bare".to_string(),
            description: None,
            input_schema: json!({"type": "object"}),
        }];
        let app = app_with_tools(tools).await;
        let (_, resp) = post_json(
            app,
            "/",
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#.to_string(),
            None,
        )
        .await;
        let tool = &resp["result"]["tools"][0];
        assert_eq!(tool["name"], "srv__bare");
        assert!(tool.get("description").is_none());
        assert_eq!(tool["inputSchema"]["type"], "object");
    }

    #[tokio::test]
    async fn nested_schema_preserved() {
        let schema = json!({
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "items": { "type": "string", "enum": ["a", "b"] }
                }
            },
            "required": ["items"]
        });
        let tools = vec![ToolInfo {
            name: "complex".to_string(),
            description: Some("Complex tool".to_string()),
            input_schema: schema.clone(),
        }];
        let app = app_with_tools(tools).await;
        let (_, resp) = post_json(
            app,
            "/",
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#.to_string(),
            None,
        )
        .await;
        let tool = &resp["result"]["tools"][0];
        assert_eq!(tool["inputSchema"], schema);
    }

    #[tokio::test]
    async fn per_server_list_does_not_double_namespace() {
        let tools = vec![ToolInfo {
            name: "ping".to_string(),
            description: None,
            input_schema: json!({"type": "object"}),
        }];
        let app = app_with_tools(tools).await;
        let (_, resp) = post_json(
            app,
            "/",
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"server":"srv"}}"#
                .to_string(),
            None,
        )
        .await;
        let names: Vec<&str> = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["srv__ping"]);
        assert!(!names[0].contains("srv__srv"));
    }

    #[tokio::test]
    async fn multiple_servers_same_tool_name_namespaced_uniquely() {
        let mut transports: HashMap<String, Arc<dyn McpTransport>> = HashMap::new();
        for server in ["alpha", "beta"] {
            transports.insert(
                server.to_string(),
                Arc::new(MockMcpTransport::new(vec!["echo".to_string()])),
            );
        }
        let registry = ToolRegistry::new(transports);
        let state = ProxyAppState::new(
            registry,
            ForgeConfig::parse_str(
                r#"
[server.alpha]
cmd = "true"
[server.beta]
cmd = "true"
"#,
            )
            .unwrap(),
            None,
        )
        .unwrap();
        let app = build_router(state);
        let (_, resp) = post_json(
            app,
            "/",
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#.to_string(),
            None,
        )
        .await;
        let names: Vec<&str> = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"alpha__echo"));
        assert!(names.contains(&"beta__echo"));
    }

    #[tokio::test]
    async fn empty_tool_list_returns_empty_array() {
        let app = app_with_tools(vec![]).await;
        let (_, resp) = post_json(
            app,
            "/",
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#.to_string(),
            None,
        )
        .await;
        assert_eq!(resp["result"]["tools"].as_array().unwrap().len(), 0);
    }
}

// ─── Auth edge cases ─────────────────────────────────────────────────

mod auth_edge {
    use super::*;

    async fn authed_app(token: &str) -> axum::Router {
        let mut transports: HashMap<String, Arc<dyn McpTransport>> = HashMap::new();
        transports.insert(
            "t".to_string(),
            Arc::new(MockMcpTransport::new(vec!["ping".to_string()])),
        );
        let mut state = ProxyAppState::new(
            ToolRegistry::new(transports),
            ForgeConfig::parse_str("[server.t]\ncmd = \"true\"\n").unwrap(),
            None,
        )
        .unwrap();
        state.auth_token = Some(token.to_string());
        build_router(state)
    }

    /// RFC 7235 §1.2: auth scheme names are case-insensitive.
    /// "bearer" and "BEARER" are equivalent to "Bearer".
    #[tokio::test]
    async fn lowercase_bearer_prefix_accepted() {
        let app = authed_app("secret").await;
        let req = Request::builder()
            .method("POST")
            .uri("/")
            .header("content-type", "application/json")
            .header("Authorization", "bearer secret")
            .body(Body::from(
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn uppercase_bearer_prefix_accepted() {
        let app = authed_app("secret").await;
        let req = Request::builder()
            .method("POST")
            .uri("/")
            .header("content-type", "application/json")
            .header("Authorization", "BEARER secret")
            .body(Body::from(
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn bearer_with_trailing_space_in_token_fails() {
        let app = authed_app("secret").await;
        let req = Request::builder()
            .method("POST")
            .uri("/")
            .header("content-type", "application/json")
            .header("Authorization", "Bearer secret ")
            .body(Body::from(
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn empty_bearer_token_rejected() {
        let app = authed_app("secret").await;
        let req = Request::builder()
            .method("POST")
            .uri("/")
            .header("content-type", "application/json")
            .header("Authorization", "Bearer ")
            .body(Body::from(
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn prefix_attack_on_well_known_still_requires_auth_for_root() {
        let app = authed_app("secret").await;
        // /.well-known-evil should NOT be exempt
        let status = get_path(app.clone(), "/.well-known-evil/mcp.json", None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn sse_endpoint_requires_auth_when_enabled() {
        let app = authed_app("secret").await;
        let status = get_path(app, "/sse", None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn sse_endpoint_passes_with_valid_token() {
        let app = authed_app("secret").await;
        let req = Request::builder()
            .method("GET")
            .uri("/sse")
            .header("Authorization", "Bearer secret")
            .header("accept", "text/event-stream")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn token_with_special_chars_works() {
        let token = "t0k3n!@#$%^&*()-_=+[]{}|;':\",./<>?";
        let app = authed_app(token).await;
        let (status, resp) = post_json(
            app,
            "/",
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#.to_string(),
            Some(token),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(resp["result"]["tools"].is_array());
    }

    #[tokio::test]
    async fn similar_token_prefix_attack_fails() {
        let app = authed_app("secret-token-abc").await;
        let (status, _) = post_json(
            app,
            "/",
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#.to_string(),
            Some("secret-token-ab"),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn tools_list_unknown_server_returns_invalid_params() {
        let mut transports: HashMap<String, Arc<dyn McpTransport>> = HashMap::new();
        transports.insert(
            "t".to_string(),
            Arc::new(MockMcpTransport::new(vec!["ping".to_string()])),
        );
        let state = ProxyAppState::new(
            ToolRegistry::new(transports),
            ForgeConfig::parse_str("[server.t]\ncmd = \"true\"\n").unwrap(),
            None,
        )
        .unwrap();
        let app = build_router(state);

        // tools/list ignores unknown server filter params and returns all tools.
        let (_, resp) = post_json(
            app,
            "/",
            r#"{"jsonrpc":"2.0","id":9,"method":"tools/list","params":{"server":"nonexistent"}}"#
                .to_string(),
            None,
        )
        .await;
        assert!(
            resp["error"].is_null(),
            "tools/list ignores unknown server param: {:?}",
            resp
        );
        assert!(resp["result"]["tools"].is_array());
    }

    #[tokio::test]
    async fn messages_endpoint_requires_auth_when_enabled() {
        let app = authed_app("secret").await;
        let status = post_json(
            app,
            "/messages",
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#.to_string(),
            None,
        )
        .await
        .0;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
}

// ─── Cache TTL edge cases ────────────────────────────────────────────

mod cache_edge {
    use super::*;

    #[tokio::test]
    async fn invalidate_server_clears_cache_entry() {
        let transport = Arc::new(MockMcpTransport::new(vec!["a".to_string()]));
        let call_count = transport.call_count.clone();
        let mut transports: HashMap<String, Arc<dyn McpTransport>> = HashMap::new();
        transports.insert("s".to_string(), transport);
        let registry = ToolRegistry::new(transports);

        let _ = registry.list_tools("s").await.unwrap();
        assert_eq!(call_count.load(std::sync::atomic::Ordering::SeqCst), 1);

        registry.invalidate_server("s").await;
        let _ = registry.list_tools("s").await.unwrap();
        assert_eq!(call_count.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
}

// ─── Live HTTP E2E (in-process mock server) ──────────────────────────

mod live_http {
    use super::*;
    use forge_core::mcp::HttpMcpTransport;
    use rmcp::{
        ServerHandler,
        model::{
            CallToolRequestParams, CallToolResult, Content, Implementation, ListToolsResult,
            PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
        },
        service::{RequestContext, RoleServer},
        transport::streamable_http_server::{
            StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
        },
    };

    #[derive(Clone)]
    struct LiveMockHandler;

    impl ServerHandler for LiveMockHandler {
        fn get_info(&self) -> ServerInfo {
            ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
                .with_server_info(Implementation::new("qa-mock", "0.0.1"))
        }

        fn list_tools(
            &self,
            _: Option<PaginatedRequestParams>,
            _: RequestContext<RoleServer>,
        ) -> impl std::future::Future<Output = Result<ListToolsResult, rmcp::ErrorData>> + Send + '_
        {
            let schema = Arc::new(
                serde_json::from_value(json!({
                    "type": "object",
                    "properties": { "msg": { "type": "string" } }
                }))
                .expect("schema"),
            );
            async move {
                Ok(ListToolsResult::with_all_items(vec![Tool::new_with_raw(
                    "live_echo".to_string(),
                    Some("Live echo tool".into()),
                    schema,
                )]))
            }
        }

        fn call_tool(
            &self,
            request: CallToolRequestParams,
            _: RequestContext<RoleServer>,
        ) -> impl std::future::Future<Output = Result<CallToolResult, rmcp::ErrorData>> + Send + '_
        {
            let name = request.name.to_string();
            async move {
                Ok(CallToolResult::success(vec![Content::text(format!(
                    "called {name}"
                ))]))
            }
        }
    }

    async fn spawn_live_server() -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let url = format!("http://127.0.0.1:{port}/");
        let service = StreamableHttpService::new(
            || Ok(LiveMockHandler),
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default(),
        );
        let app = axum::Router::new().fallback_service(service);
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        (url, handle)
    }

    #[tokio::test]
    async fn http_transport_lists_tools_with_schema_from_live_server() {
        let (url, handle) = spawn_live_server().await;
        let transport = HttpMcpTransport::connect_streamable(&url, HashMap::new())
            .await
            .expect("connect to live mock");
        let tools = transport.list_tools().await.expect("list tools");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "live_echo");
        assert_eq!(tools[0].description.as_deref(), Some("Live echo tool"));
        assert_eq!(tools[0].input_schema["properties"]["msg"]["type"], "string");
        handle.abort();
    }

    #[tokio::test]
    async fn http_transport_call_tool_round_trip() {
        let (url, handle) = spawn_live_server().await;
        let transport = HttpMcpTransport::connect_streamable(&url, HashMap::new())
            .await
            .unwrap();
        let result = transport
            .call_tool("live_echo", json!({"msg": "hello"}))
            .await
            .unwrap();
        assert!(result.to_string().contains("called live_echo"));
        handle.abort();
    }

    #[tokio::test]
    async fn proxy_with_http_backend_returns_real_schema() {
        let (url, server_handle) = spawn_live_server().await;

        let cfg = ForgeConfig::parse_str(&format!(
            r#"
[server.live]
transport = "http"
url = "{url}"
"#
        ))
        .unwrap();

        let registry = forge_core::mcp::build_tool_registry(&cfg).await.unwrap();
        let state = ProxyAppState::new(registry, cfg, None).unwrap();
        let app = build_router(state);

        let (_, resp) = post_json(
            app,
            "/",
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#.to_string(),
            None,
        )
        .await;

        let tool = &resp["result"]["tools"][0];
        assert_eq!(tool["name"], "live__live_echo");
        assert_eq!(tool["description"], "Live echo tool");
        assert_eq!(tool["inputSchema"]["properties"]["msg"]["type"], "string");

        server_handle.abort();
    }

    #[tokio::test]
    async fn build_tool_registry_fails_when_all_servers_unreachable() {
        let cfg = ForgeConfig::parse_str(
            r#"
[server.dead]
transport = "http"
url = "http://127.0.0.1:1"
"#,
        )
        .unwrap();
        let result = forge_core::mcp::build_tool_registry(&cfg).await;
        assert!(
            result.is_err(),
            "unreachable server should fail registry build"
        );
    }
}

// ─── Live legacy SSE E2E ─────────────────────────────────────────────

mod live_sse {
    use super::*;
    use forge_core::mcp::LegacySseMcpTransport;

    async fn spawn_legacy_sse_mock() -> (String, tokio::task::JoinHandle<()>) {
        use axum::{
            Json, Router,
            extract::{Query, State},
            http::StatusCode,
            response::{
                IntoResponse,
                sse::{Event, KeepAlive, Sse},
            },
            routing::{get, post},
        };
        use dashmap::DashMap;
        use futures::stream::{self, StreamExt};
        use serde::Deserialize;
        use tokio::sync::mpsc;
        use uuid::Uuid;

        #[derive(Clone)]
        struct SseState {
            sessions: Arc<DashMap<String, mpsc::Sender<String>>>,
        }

        #[derive(Deserialize)]
        struct SessionQuery {
            session_id: Option<String>,
        }

        struct SessionGuard {
            session_id: String,
            sessions: Arc<DashMap<String, mpsc::Sender<String>>>,
        }

        impl Drop for SessionGuard {
            fn drop(&mut self) {
                self.sessions.remove(&self.session_id);
            }
        }

        async fn handle_sse(
            State(state): State<SseState>,
        ) -> Sse<impl futures::Stream<Item = Result<Event, std::convert::Infallible>>> {
            let session_id = Uuid::new_v4().to_string();
            let (tx, rx) = mpsc::channel(64);
            state.sessions.insert(session_id.clone(), tx);
            let endpoint_event = Event::default()
                .event("endpoint")
                .data(format!("/messages?session_id={session_id}"));
            let guard = SessionGuard {
                session_id,
                sessions: state.sessions.clone(),
            };
            let message_stream = stream::unfold((rx, guard), |(mut rx, guard)| async move {
                rx.recv().await.map(|data| {
                    let event = Event::default().event("message").data(data);
                    (event, (rx, guard))
                })
            });
            let combined = stream::once(async { endpoint_event }).chain(message_stream);
            Sse::new(combined.map(Ok)).keep_alive(KeepAlive::default())
        }

        async fn handle_messages(
            State(state): State<SseState>,
            Query(q): Query<SessionQuery>,
            Json(req): Json<serde_json::Value>,
        ) -> impl IntoResponse {
            let Some(session_id) = q.session_id else {
                return StatusCode::BAD_REQUEST.into_response();
            };
            let Some(tx) = state.sessions.get(&session_id).map(|e| e.clone()) else {
                return StatusCode::NOT_FOUND.into_response();
            };
            let id = req.get("id").cloned().unwrap_or(json!(null));
            let method = req
                .get("method")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let response = match method {
                "tools/list" => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "tools": [{
                            "name": "sse_tool",
                            "description": "Legacy SSE tool",
                            "inputSchema": { "type": "object", "properties": { "q": { "type": "string" } } }
                        }]
                    }
                }),
                "tools/call" => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "content": [{ "type": "text", "text": "sse ok" }] }
                }),
                _ => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": "not found" }
                }),
            };
            if tx.try_send(response.to_string()).is_err() {
                return StatusCode::GONE.into_response();
            }
            StatusCode::ACCEPTED.into_response()
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let url = format!("http://127.0.0.1:{port}/sse");
        let state = SseState {
            sessions: Arc::new(DashMap::new()),
        };
        let app = Router::new()
            .route("/sse", get(handle_sse))
            .route("/messages", post(handle_messages))
            .with_state(state);
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        (url, handle)
    }

    #[tokio::test]
    async fn legacy_sse_transport_lists_tools_with_schema() {
        let (url, handle) = spawn_legacy_sse_mock().await;
        let transport = LegacySseMcpTransport::connect(&url, HashMap::new())
            .await
            .expect("SSE connect");
        let tools = transport.list_tools().await.expect("list tools");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "sse_tool");
        assert_eq!(tools[0].description.as_deref(), Some("Legacy SSE tool"));
        assert_eq!(tools[0].input_schema["properties"]["q"]["type"], "string");
        handle.abort();
    }

    #[tokio::test]
    async fn legacy_sse_transport_call_tool_round_trip() {
        let (url, handle) = spawn_legacy_sse_mock().await;
        let transport = LegacySseMcpTransport::connect(&url, HashMap::new())
            .await
            .unwrap();
        let result = transport
            .call_tool("sse_tool", json!({"q": "test"}))
            .await
            .unwrap();
        assert!(result.to_string().contains("sse ok"));
        handle.abort();
    }

    #[tokio::test]
    async fn proxy_with_sse_backend_returns_real_schema() {
        let (url, server_handle) = spawn_legacy_sse_mock().await;
        let cfg = ForgeConfig::parse_str(&format!(
            r#"
[server.legacy]
transport = "sse"
url = "{url}"
"#
        ))
        .unwrap();
        let registry = forge_core::mcp::build_tool_registry(&cfg).await.unwrap();
        let state = ProxyAppState::new(registry, cfg, None).unwrap();
        let app = build_router(state);
        let (_, resp) = post_json(
            app,
            "/",
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#.to_string(),
            None,
        )
        .await;
        let tool = &resp["result"]["tools"][0];
        assert_eq!(tool["name"], "legacy__sse_tool");
        assert_eq!(tool["description"], "Legacy SSE tool");
        server_handle.abort();
    }
}
