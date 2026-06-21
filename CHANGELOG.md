# Changelog

All notable changes to MCP Forge are documented here.

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versions follow [Semantic Versioning](https://semver.org/).

---

## [Unreleased] — v1.0.0

### Added

- **Schema passthrough** — `tools/list` returns real `description` and `inputSchema` from upstream MCP servers.
- **HTTP transport** — connect to remote MCP servers via `transport = "http"` (Streamable HTTP).
- **Legacy SSE transport** — connect via `transport = "sse"` (MCP 2024-11-05). Marked stable; prefer `http` for new integrations.
- **Proxy Bearer auth** — optional `[proxy] auth_token`; constant-time comparison; `/.well-known/` exempt.
- **`forge-mock-mcp --http`** — Streamable HTTP mock server for E2E testing.
- **`forge-mock-mcp --sse`** — legacy SSE mock server for E2E testing.
- **QA regression suite** — `crates/forge-proxy/tests/qa_edge_cases.rs` runs in CI.

### Fixed

- **`forge check`** now probes HTTP/SSE servers for live MCP reachability (not just URL presence).
- **`forge status`** shows remote endpoint URLs for HTTP/SSE backends after proxy start.
- **`tools/list` with unknown `params.server`** now returns JSON-RPC `-32602` (invalid params) instead of `-32603`.
- Invalid `FORGE_TOOL_CACHE_TTL_SECS` values log a warning and fall back to the 60s default.

### Changed

- **Default tool cache TTL** reduced from 300s to 60s (`FORGE_TOOL_CACHE_TTL_SECS` still overrides).
- **`ServerConfig.cmd`** is now optional (required only for `transport = "stdio"`).
- **`forge-mock-mcp` stdio tools** — default two tools remain **`echo`** and **`ping`** for backward compatibility; additional tools use `tool_2`, `tool_3`, …

---

### Fixed

- Support `v`-prefixed release tags in the CI release workflow so Homebrew formula auto-updates correctly on tag push.
- Removed plaintext secret key names from log sinks (CodeQL CleartextLogging).
- Fixed polling-based stale cache test reliability.
- Documented `--fix` interactivity requirement for clippy auto-fix.
- Use `build-mode: none` for CodeQL Rust analysis to avoid redundant builds.

---

## [0.1.0] — 2026-04-03

### Added

- **Multi-server proxy** — single local endpoint (`127.0.0.1:3456`) that routes to multiple MCP servers with `server__tool` namespacing.
- **Process supervisor** — `forge start/stop/restart/status/logs` lifecycle commands for all registered servers.
- **Config workflow** — `forge init`, `forge add`, `forge remove`, `forge ls`, `forge check` for managing `forge.toml`.
- **Guard system** — opt-in prompt injection detection (`warn` or `block` mode), per-server rate limits (`max_calls_per_min`), and daily call caps (`max_calls_per_day`).
- **RBAC-style policy** — `allowed_tools` and `deny_tools` per server; deny always wins.
- **SQLite audit log** — all tool calls, policy denials, and guard events recorded; queryable via `forge audit` and summarised by `forge report`.
- **Secret management** — `forge secret set/ls/rm/check`; secrets resolved from env vars or OS keychain, never stored in plaintext.
- **Discovery endpoint** — `/.well-known/mcp-servers.json` lists active servers and their tools.
- **Homebrew distribution** — `brew install devesrawat/mcp-forge/mcp-forge`.
- **Shell installer** — one-line `curl | sh` install.
