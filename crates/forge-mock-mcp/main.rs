//! Minimal MCP stub for CI and local testing (stdio, Streamable HTTP, or legacy SSE).
use clap::Parser;
use rmcp::{
    ServerHandler,
    model::{
        CallToolRequestParams, CallToolResult, Content, Implementation, JsonObject,
        ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
    },
    service::{RequestContext, RoleServer},
};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::sync::Arc;

#[derive(Parser)]
struct Args {
    /// Run as a Streamable HTTP MCP server on the given address (e.g., 127.0.0.1:9999)
    #[arg(long, conflicts_with = "sse")]
    http: Option<String>,

    /// Run as a legacy SSE MCP server (MCP 2024-11-05) on the given address
    #[arg(long, conflicts_with = "http")]
    sse: Option<String>,

    /// Number of fake tools to expose (default: 2)
    #[arg(long, default_value = "2")]
    tools: usize,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if let Some(addr) = args.sse {
        run_sse_server(&addr, args.tools)
    } else if let Some(addr) = args.http {
        run_http_server(&addr, args.tools)
    } else {
        run_stdio_server(args.tools)
    }
}

fn tool_names(count: usize) -> Vec<String> {
    const LEGACY: [&str; 2] = ["echo", "ping"];
    (0..count)
        .map(|i| {
            LEGACY
                .get(i)
                .map(|name| (*name).to_string())
                .unwrap_or_else(|| format!("tool_{i}"))
        })
        .collect()
}

fn empty_object_schema() -> Arc<JsonObject> {
    Arc::new(serde_json::from_value(json!({ "type": "object" })).expect("valid schema"))
}

fn dispatch_jsonrpc(names: &[String], req: &Value) -> Option<Value> {
    let id = req.get("id").cloned().unwrap_or(json!(null));
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");

    let response = match method {
        "initialize" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "serverInfo": { "name": "forge-mock-mcp", "version": env!("CARGO_PKG_VERSION") }
            }
        }),
        "notifications/initialized" => return None,
        "tools/list" => {
            let tools: Vec<Value> = names
                .iter()
                .map(|name| json!({ "name": name, "inputSchema": { "type": "object" } }))
                .collect();
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "tools": tools }
            })
        }
        "tools/call" => {
            let name = req
                .pointer("/params/name")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [{ "type": "text", "text": format!("ok from {name}") }],
                    "isError": false
                }
            })
        }
        _ => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": "method not found" }
        }),
    };
    Some(response)
}

#[derive(Clone)]
struct MockHandler {
    tool_names: Vec<String>,
}

impl MockHandler {
    fn new(tool_names: Vec<String>) -> Self {
        Self { tool_names }
    }

    fn tools(&self) -> Vec<Tool> {
        let schema = empty_object_schema();
        self.tool_names
            .iter()
            .map(|name| Tool::new_with_raw(name.clone(), None, schema.clone()))
            .collect()
    }
}

impl ServerHandler for MockHandler {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            Implementation::new("forge-mock-mcp", env!("CARGO_PKG_VERSION")),
        )
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListToolsResult, rmcp::ErrorData>> + Send + '_
    {
        let tools = self.tools();
        async move { Ok(ListToolsResult::with_all_items(tools)) }
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<CallToolResult, rmcp::ErrorData>> + Send + '_
    {
        let name = request.name.to_string();
        async move {
            Ok(CallToolResult::success(vec![Content::text(format!(
                "ok from {name}"
            ))]))
        }
    }
}

fn run_stdio_server(tool_count: usize) -> anyhow::Result<()> {
    let names = tool_names(tool_count);
    let stdin = std::io::stdin().lock();
    let mut reader = BufReader::new(stdin);
    let mut stdout = std::io::stdout().lock();
    let mut line = String::new();

    while reader.read_line(&mut line)? > 0 {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            line.clear();
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(trimmed) else {
            line.clear();
            continue;
        };
        if let Some(response) = dispatch_jsonrpc(&names, &req) {
            writeln!(stdout, "{}", serde_json::to_string(&response)?)?;
            stdout.flush()?;
        }
        line.clear();
    }

    Ok(())
}

fn run_http_server(addr: &str, tool_count: usize) -> anyhow::Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            use axum::Router;
            use rmcp::transport::streamable_http_server::{
                StreamableHttpServerConfig, StreamableHttpService,
                session::local::LocalSessionManager,
            };

            let session_manager = Arc::new(LocalSessionManager::default());
            let tool_names = tool_names(tool_count);

            let service = StreamableHttpService::new(
                move || Ok(MockHandler::new(tool_names.clone())),
                session_manager,
                StreamableHttpServerConfig::default(),
            );

            let app = Router::new().fallback_service(service);
            let listener = tokio::net::TcpListener::bind(addr).await?;
            eprintln!("forge-mock-mcp HTTP server listening on {addr}");
            axum::serve(listener, app).await?;
            Ok(())
        })
}

fn run_sse_server(addr: &str, tool_count: usize) -> anyhow::Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async move {
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
                tool_names: Arc<Vec<String>>,
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
            ) -> Sse<impl futures::Stream<Item = Result<Event, std::convert::Infallible>>>
            {
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
                Json(req): Json<Value>,
            ) -> impl IntoResponse {
                let Some(session_id) = q.session_id else {
                    return (StatusCode::BAD_REQUEST, "missing session_id").into_response();
                };
                let Some(tx) = state.sessions.get(&session_id).map(|e| e.clone()) else {
                    return (StatusCode::NOT_FOUND, "unknown session").into_response();
                };

                let Some(response) = dispatch_jsonrpc(state.tool_names.as_slice(), &req) else {
                    return StatusCode::ACCEPTED.into_response();
                };

                let payload = serde_json::to_string(&response).unwrap_or_default();
                if tx.try_send(payload).is_err() {
                    return StatusCode::GONE.into_response();
                }
                StatusCode::ACCEPTED.into_response()
            }

            let state = SseState {
                tool_names: Arc::new(tool_names(tool_count)),
                sessions: Arc::new(DashMap::new()),
            };

            let app = Router::new()
                .route("/sse", get(handle_sse))
                .route("/messages", post(handle_messages))
                .with_state(state);

            let listener = tokio::net::TcpListener::bind(addr).await?;
            eprintln!("forge-mock-mcp legacy SSE server listening on {addr} (GET /sse)");
            axum::serve(listener, app).await?;
            Ok(())
        })
}
