# MCP Forge

[![CI](https://github.com/devesrawat/MCPForge/actions/workflows/ci.yml/badge.svg)](https://github.com/devesrawat/MCPForge/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENCE)

MCP Forge is a Rust CLI and local proxy that helps you run and govern multiple MCP servers through one endpoint.

It focuses on practical operations: process supervision, tool namespacing, policy enforcement, rate and cost guards, and audit visibility.

## Why MCP Forge

- One local endpoint for many MCP servers.
- Safer tool execution with allow/deny policies and injection checks.
- Operational controls (rate limits, daily caps, status, logs, restart).
- Audit trail and reports for usage and latency insights.

## Installation

### Homebrew

```bash
brew install devesrawat/mcp-forge/mcp-forge
```

### Shell installer

```bash
curl -fsSL https://raw.githubusercontent.com/devesrawat/MCPForge/main/install.sh | sh
```

### Build from source

```bash
git clone https://github.com/devesrawat/MCPForge.git
cd MCPForge
cargo build --release --bin forge
cp target/release/forge ~/.local/bin/
```

### Cargo install

```bash
cargo install --path crates/forge-cli
```

## Quickstart

### 1) Add a server

```bash
forge add github --cmd "npx -y @modelcontextprotocol/server-github"
```

### 2) Start Forge

```bash
forge start
```

### 3) Check status

```bash
forge status
```

### 4) Connect your MCP client

```text
http://127.0.0.1:3456
```

## Real-Life Usage Examples

### Example 1: Run multiple MCP servers behind one endpoint

Problem: Your agent/client needs tools from more than one MCP server, but managing each server separately is noisy.

```bash
forge add github --cmd "npx -y @modelcontextprotocol/server-github"
forge add filesystem --cmd "npx -y @modelcontextprotocol/server-filesystem /tmp"
forge start
forge status
```

Use a single MCP endpoint in your client:

```text
http://127.0.0.1:3456
```

What you get: one stable endpoint with namespaced tools like `github__*` and `filesystem__*`.

### Example 2: Block unsafe tool calls in local development

Problem: You want guardrails so suspicious prompts or tool arguments do not silently execute.

In your `forge.toml`:

```toml
[guard]
enabled = true
injection_mode = "block"

[server.github]
cmd = "npx -y @modelcontextprotocol/server-github"
transport = "stdio"
allowed_tools = ["search_repositories", "get_file_contents"]
deny_tools = ["delete_file", "create_or_update_file"]
```

Then validate and start:

```bash
forge check
forge start
```

What you get: deny-first policy enforcement plus injection blocking for safer experimentation.

### Example 3: Prevent runaway usage with limits and daily caps

Problem: A noisy loop or aggressive agent behavior can call tools too often and inflate costs.

In your `forge.toml`:

```toml
[server.github]
cmd = "npx -y @modelcontextprotocol/server-github"
transport = "stdio"
max_calls_per_min = 30
max_calls_per_day = 2000
```

Inspect usage:

```bash
forge audit
forge report
```

What you get: predictable usage, explicit limit errors, and visibility into tool call volume and latency.

### Example 4: Daily operator workflow

```bash
# start local stack
forge start

# check health
forge status --watch

# investigate events or failures
forge logs github --follow

# stop cleanly when done
forge stop
```

## Configuration

Start from [forge.toml.example](forge.toml.example) or run `forge init` to generate a config.

Minimal example:

```toml
[proxy]
enabled = true
bind = "127.0.0.1"
port = 3456

[guard]
enabled = true
injection_mode = "block" # "warn" or "block"

[server.github]
cmd = "npx -y @modelcontextprotocol/server-github"
transport = "stdio"
secret.GH_TOKEN = "env:GITHUB_TOKEN"
max_calls_per_min = 60
max_calls_per_day = 10000
```

Validate config:

```bash
forge check
```

## CLI Commands

| Command | Description |
|---------|-------------|
| `forge init` | Generate a starter `forge.toml` |
| `forge add <name> --cmd <cmd>` | Register a new MCP server |
| `forge remove <name>` | Remove a registered server |
| `forge ls` | List all configured servers |
| `forge start` | Start all servers and the proxy |
| `forge stop` | Stop all servers and the proxy |
| `forge restart [name]` | Restart one or all servers |
| `forge status [--watch] [--json]` | Show server health |
| `forge logs <name> [--follow] [--lines N]` | Stream or tail server logs |
| `forge secret <set\|ls\|rm\|check>` | Manage secrets in keychain/env |
| `forge check` | Validate config and secrets |
| `forge audit` | Query the audit event log |
| `forge report` | Summarise usage and latency |

## Security Model

- Local binding defaults to `127.0.0.1`.
- Request size and timeout constraints in proxy.
- Optional injection checks for arguments and results.
- Policy deny events are audit logged.
- Secrets resolved via env/keychain references — never stored in plaintext.

**Guards are disabled by default** (`guard.enabled = false`). Set `guard.enabled = true` to enable injection, rate, and cost guard checks. RBAC policy enforcement (`allowed_tools`, `deny_tools`) always applies regardless of this setting.

## Project Layout

```
crates/
  forge-cli/    CLI entrypoint and command handlers
  forge-core/   Config, supervisor, protocol, secrets, audit
  forge-proxy/  Axum JSON-RPC proxy and guard logic
  forge-mock-mcp/ Mock MCP server for local tests
forge.toml.example  Sample configuration
```

## Development

### Prerequisites

- Rust stable toolchain (`rustup install stable`)
- `cargo`

### Build

```bash
cargo build --workspace
```

### Test

```bash
cargo test --all
```

### Lint

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
```

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## Changelog

See [CHANGELOG.md](CHANGELOG.md).

## License

MIT. See [LICENCE](LICENCE).
