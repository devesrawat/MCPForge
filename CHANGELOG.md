# Changelog

All notable changes to MCP Forge are documented here.

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versions follow [Semantic Versioning](https://semver.org/).

---

## [Unreleased]

### Added

- Rate-limit, cost-limit, and prompt-injection-blocked tool calls are
  now written to the audit trail (previously only RBAC policy denials
  were logged) — `forge audit`/`forge report`/`forge watch` now show
  the full picture of what an agent attempted, not just what succeeded
  plus policy denials.
- `forge watch`'s status column now shows "rate-limited" and
  "cost-limited" labels instead of a raw result code.
- `forge report --format markdown`: new export format with a
  denials-by-reason breakdown, suitable for sharing directly with a
  client as a compliance artifact.
- `forge add --preset <github|filesystem>`: applies a curated,
  source-verified `deny_tools` list of mutating operations for that
  server type, so a new server starts with real protection instead of
  an empty policy. Omitting `--preset` is unchanged (empty `deny_tools`).
- New destructive-pattern guard: tool calls whose name (e.g.
  `delete_*`, `drop_*`, `force_*`) or arguments (e.g. `rm -rf`,
  `DROP TABLE`) match a known destructive pattern are blocked by
  default when `guard.enabled = true`, configurable via
  `guard.destructive_pattern_mode = "warn"|"block"` (default `"block"`).
  Blocked calls are audit-logged and show up in `forge report`'s
  denials-by-reason breakdown and `forge watch`'s "Blocked" filter,
  consistent with the other guard-triggered denial reasons.

### Fixed

- Keychain secrets now scope to the active `FORGE_HOME` override
  (`mcp-forge:<forge_home>` instead of a single global `mcp-forge`
  service), so different clients run with different `FORGE_HOME` values
  no longer collide on the same OS keychain entry. Secrets stored before
  this change (under the default, unscoped `FORGE_HOME`) continue to
  resolve unchanged.
- `forge report`: added a `--config` flag (matching `forge start`/`forge
  check`); the per-call cost map is now read from the config actually in
  use instead of a hardcoded `forge.toml` in the current directory.

## [0.1.2] — 2026-06-21

### Fixed

- `forge status`: stale-PID detection now uses `libc::kill(2)` directly instead of the shell `kill` command, preventing `u32::MAX` from being cast to `pid_t -1` and falsely reporting processes as alive.
- `forge stop`: same `libc::kill(2)` fix for PID liveness probe; also guards against `pid == 0` and `pid > i32::MAX`.
- JSON-RPC proxy: added `/.well-known/mcp` discovery endpoint (MCP spec §4.2); was returning 404.
- JSON-RPC proxy: batch requests (`[{…}, {…}]`) now return a batch response array; previously returned a single-object error.
- JSON-RPC proxy: notifications (objects without `"id"`) are forwarded without generating a response; previously the proxy incorrectly echoed a response.
- JSON-RPC proxy: parse errors now return correct `{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"…"}}` instead of HTTP 422.
- Auth middleware: constant-time comparison prevents timing side-channel on token checks; wrong/missing token returns JSON-RPC `-32001` with `application/json` content-type.
- `forge add`: rejects empty or whitespace-only `--cmd` for stdio servers; rejects `--transport http/sse` without a `--url`.
- `forge add`: new `--transport` flag accepts `stdio`, `http`, `sse`; new `--url` flag for HTTP/SSE transports.
- `forge check`: accepts `--config` flag to specify config path (was hardcoded).
- `forge status`: project-scoped output — only shows servers defined in the local `forge.toml`; falls back to global view when no config file found.
- `forge stop`: user-friendly error when no daemon is running ("no running forge proxy found. Start one with: forge start --daemon").
- `forge logs`: error now explains daemon-mode requirement instead of exposing internal socket path.
- `forge secret set`: accepts `--value <VALUE>` flag or `FORGE_SECRET_VALUE` env var for non-interactive use.
- `forge report`: latency formatted as `{:.2}ms` (was truncating to integer); cost formatted without leading space.
- `forge audit`: shows `<1ms` for zero-stored latency rather than `0ms`.
- Tool schema passthrough: `inputSchema` from upstream MCP servers is preserved in `tools/list` responses.
- HTTP/SSE transport: `forge add` can register remote MCP servers over HTTP Streamable or legacy SSE transport.
- Proxy `test_helpers` module: removed duplicate inline `pub mod test_helpers` block that caused E0428 compile error when running `cargo test`.

---

## [0.1.1] — 2026-04-04

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
