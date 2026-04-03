//! Minimal newline-delimited JSON-RPC MCP stub for CI and local testing.
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};

fn main() -> anyhow::Result<()> {
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
            "notifications/initialized" => {
                line.clear();
                continue;
            }
            "tools/list" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "tools": [
                        { "name": "echo", "inputSchema": { "type": "object" } },
                        { "name": "ping", "inputSchema": { "type": "object" } }
                    ]
                }
            }),
            "tools/call" => {
                let name = req
                    .pointer("/params/name")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{ "type": "text", "text": format!("ok from {}", name) }],
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

        writeln!(stdout, "{}", serde_json::to_string(&response)?)?;
        stdout.flush()?;
        line.clear();
    }

    Ok(())
}
