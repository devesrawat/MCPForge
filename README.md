# MCP Forge

MCP Forge is a Rust CLI and local proxy that helps you run and govern multiple MCP servers through one endpoint.

It focuses on practical operations: process supervision, tool namespacing, policy enforcement, rate and cost guards, and audit visibility.

## Why MCP Forge

- One local endpoint for many MCP servers.
- Safer tool execution with allow/deny policies and injection checks.
- Operational controls (rate limits, daily caps, status, logs, restart).
- Audit trail and reports for usage and latency insights.

## Current Status

- Language: Rust (workspace)
- License: MIT
- Primary branch: `development`
- Project plan: [PLAN.md](PLAN.md)

## Features

- Multi-server proxy with namespaced tools (`server__tool`).
- Supervisor lifecycle commands: start, stop, restart, status, logs.
- Config and secret workflows: init, add/remove/list, keychain/env checks.
- Guard controls: prompt injection mode (`warn` or `block`), per-server rate limit (`max_calls_per_min`), and per-server daily cap (`max_calls_per_day`).
- RBAC-style tool policy (`allowed_tools`, `deny_tools`, deny wins).
- SQLite audit storage and summary reporting.
- Discovery endpoint: `/.well-known/mcp-servers.json`.

## Installation

### Shell installer

```bash
curl -fsSL https://raw.githubusercontent.com/devesrawat/MCPForge/main/install.sh | sh
```

### Homebrew (after first tagged release)

```bash
brew tap devesrawat/mcp-forge
brew install forge
```

### Build from source

```bash
git clone https://github.com/devesrawat/MCPForge.git
cd MCPForge
cargo build --release --bin forge
```

### Cargo install (local repo path)

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

Use:

```text
http://127.0.0.1:3456
```

## Real-Life Usage Examples

These examples show when MCP Forge is useful and how a new user can apply it immediately.

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

What you get: clear operational lifecycle for local MCP infrastructure.

## Configuration

Start from [forge.toml.example](forge.toml.example).

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

Core commands currently available:

- `forge init`
- `forge add`
- `forge ls`
- `forge remove`
- `forge start`
- `forge stop`
- `forge restart`
- `forge status [--watch] [--json]`
- `forge logs <server> [--follow] [--lines N]`
- `forge secret <set|ls|rm|check>`
- `forge check`
- `forge audit`
- `forge report`

## Project Layout

- `crates/forge-cli` - CLI entrypoint and command handlers.
- `crates/forge-core` - config, supervisor, protocol, secrets, audit.
- `crates/forge-proxy` - axum JSON-RPC proxy and guard logic.
- `crates/forge-mock-mcp` - mock MCP server for local tests.
- `forge.toml.example` - sample config.
- `RELEASE.md` - release runbook and rollback guide.

## Security Model

- Local binding defaults to `127.0.0.1`.
- Request size and timeout constraints in proxy.
- Optional injection checks for arguments and results.
- Policy deny events are audit logged.
- Secrets resolved via env/keychain references.

Note: `guard.enabled = false` disables injection, rate, and cost guard checks. RBAC policy enforcement still applies. Use for trusted local development only.

## Development

### Prerequisites

- Rust stable toolchain
- `cargo`

### Build

```bash
cargo build --workspace
```

### Validate

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all
```

## Release

Tag-driven release workflow:

- Workflow: [.github/workflows/release.yml](.github/workflows/release.yml)
- Runbook: [RELEASE.md](RELEASE.md)
- Real-life release examples: [RELEASE.md#real-life-examples](RELEASE.md#real-life-examples)
- Helper script: `./scripts/release-tag.sh vX.Y.Z`

## Roadmap

Execution plan and milestones are documented in [PLAN.md](PLAN.md).

## Contributing

Contribution basics are in [CONTRIBUTING.md](CONTRIBUTING.md).

If you submit code changes, please include tests and keep `fmt`, `clippy`, and `test` green.

## License

MIT. See [LICENCE](LICENCE).
