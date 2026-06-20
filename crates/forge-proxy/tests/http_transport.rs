use std::sync::Arc;

// ── in-process mock HTTP MCP server ──────────────────────────────────────────

#[derive(Clone)]
struct MockHandler {
    tool_names: Vec<String>,
}

impl rmcp::ServerHandler for MockHandler {
    fn get_info(&self) -> rmcp::model::ServerInfo {
        rmcp::model::ServerInfo::new(
            rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .build(),
        )
        .with_server_info(rmcp::model::Implementation::new("test-mock", "0.0.0"))
    }

    fn list_tools(
        &self,
        _req: Option<rmcp::model::PaginatedRequestParams>,
        _ctx: rmcp::service::RequestContext<rmcp::service::RoleServer>,
    ) -> impl std::future::Future<Output = Result<rmcp::model::ListToolsResult, rmcp::ErrorData>>
           + Send
           + '_ {
        let schema: Arc<rmcp::model::JsonObject> = Arc::new(
            serde_json::from_value(serde_json::json!({"type":"object","properties":{}})).unwrap(),
        );
        let tools: Vec<rmcp::model::Tool> = self
            .tool_names
            .iter()
            .map(|n| rmcp::model::Tool::new_with_raw(n.clone(), None, schema.clone()))
            .collect();
        async move { Ok(rmcp::model::ListToolsResult::with_all_items(tools)) }
    }

    fn call_tool(
        &self,
        req: rmcp::model::CallToolRequestParams,
        _ctx: rmcp::service::RequestContext<rmcp::service::RoleServer>,
    ) -> impl std::future::Future<Output = Result<rmcp::model::CallToolResult, rmcp::ErrorData>>
           + Send
           + '_ {
        let name = req.name.to_string();
        async move {
            Ok(rmcp::model::CallToolResult::success(vec![
                rmcp::model::Content::text(format!("ok:{name}")),
            ]))
        }
    }
}

async fn start_mock_http_server(tool_names: Vec<String>) -> std::net::SocketAddr {
    use axum::Router;
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService,
        session::local::LocalSessionManager,
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let session_manager = Arc::new(LocalSessionManager::default());
    let service = StreamableHttpService::new(
        move || Ok(MockHandler { tool_names: tool_names.clone() }),
        session_manager,
        StreamableHttpServerConfig::default(),
    );

    let app = Router::new().fallback_service(service);
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });

    addr
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_e2e_http_transport_tools_list() {
    let addr = start_mock_http_server(vec!["alpha".to_string(), "beta".to_string()]).await;
    let url = format!("http://{addr}");

    let config_toml = format!(
        r#"
[server.mock]
transport = "http"
url = "{url}"
"#
    );

    let cfg = forge_core::config::ForgeConfig::parse_str(&config_toml).unwrap();
    let registry = forge_core::mcp::build_tool_registry(&cfg).await.unwrap();
    let tools = registry.list_all_tools().await.unwrap();

    assert_eq!(tools.len(), 2, "expected 2 tools, got: {:?}", tools);
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"mock__alpha"), "names: {:?}", names);
    assert!(names.contains(&"mock__beta"), "names: {:?}", names);
}

#[tokio::test]
async fn test_e2e_http_transport_tool_call() {
    let addr = start_mock_http_server(vec!["greet".to_string()]).await;
    let url = format!("http://{addr}");

    let config_toml = format!(
        r#"
[server.svc]
transport = "http"
url = "{url}"
"#
    );

    let cfg = forge_core::config::ForgeConfig::parse_str(&config_toml).unwrap();
    let registry = forge_core::mcp::build_tool_registry(&cfg).await.unwrap();
    let result = registry
        .call_tool("svc__greet", serde_json::json!({}))
        .await
        .unwrap();

    let text = result["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("greet"), "unexpected result: {}", text);
}

#[tokio::test]
async fn test_config_http_server_without_url_fails_validation() {
    let result = forge_core::config::ForgeConfig::parse_str(
        r#"
[server.broken]
transport = "http"
"#,
    );
    assert!(result.is_err(), "http server without url should fail config validation");
}

#[tokio::test]
async fn test_config_sse_server_without_url_fails_validation() {
    let result = forge_core::config::ForgeConfig::parse_str(
        r#"
[server.broken]
transport = "sse"
"#,
    );
    assert!(result.is_err(), "sse server without url should fail config validation");
}
