# mcp-forge QA Bug Report

**Date:** 2026-06-21  
**Tester:** Systematic QA — manual black-box testing of release binary  
**Binary:** `target/release/forge` (built from `development` branch, commit `1d77f2b`)  
**Environment:** macOS Darwin 25.5.0, zsh  
**Test directory:** `/tmp/forge_qa_test/`  
**Mock MCP server:** `target/release/forge-mock-mcp`

---

## Summary

| Severity | Count |
|----------|-------|
| CRITICAL | 3     |
| HIGH     | 7     |
| MEDIUM   | 5     |
| LOW      | 5     |
| **TOTAL**| **20**|

---

## CRITICAL

---

### BUG-01 — `forge --version` not implemented

**Severity:** CRITICAL  
**Component:** CLI / UX  
**Reproducible:** Always

**Steps to reproduce:**
```sh
forge --version
```

**Expected:**
```
forge 0.1.1
```

**Actual:**
```
error: unexpected argument '--version' found
```

**Impact:** Every CLI tool must respond to `--version` and `-V`. This is the most basic CLI convention. Users cannot check which version is installed, cannot file accurate bug reports, and cannot pin versions in CI scripts. `clap` makes this a one-liner with `.version(env!("CARGO_PKG_VERSION"))`.

---

### BUG-09 — `/.well-known/mcp` returns 404

**Severity:** CRITICAL  
**Component:** Proxy / MCP Protocol Compliance  
**Reproducible:** Always (proxy running)

**Steps to reproduce:**
```sh
forge start
curl -sv http://127.0.0.1:3456/.well-known/mcp
```

**Expected:** HTTP 200 with MCP server descriptor JSON (per MCP Streamable HTTP spec).

**Actual:**
```
< HTTP/1.1 404 Not Found
< content-length: 0
```

**Impact:** The `/.well-known/mcp` endpoint is the MCP service discovery mechanism. Without it, MCP-compliant client implementations cannot auto-discover the proxy. Any client following the spec will fail to connect without manual configuration. Breaks out-of-the-box compatibility with tools like Claude Desktop, Cursor, and other MCP clients that rely on discovery.

---

### BUG-12 — `tools/list` leaks denied/restricted tools

**Severity:** CRITICAL  
**Component:** Proxy / Security / Policy Enforcement  
**Reproducible:** Always when `allowed_tools` or `deny_tools` are configured

**Steps to reproduce:**
```toml
# forge.toml
[server.mock_echo]
allowed_tools = ["ping"]
deny_tools = ["echo"]
```
```sh
forge start
curl -s -X POST http://127.0.0.1:3456/ -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}'
```

**Expected:**
```json
{"tools": [{"name": "mock_echo__ping"}]}
```
(Only allowed tools should appear.)

**Actual:**
```json
{"tools": [{"name": "mock_echo__echo"}, {"name": "mock_echo__ping"}]}
```
(Both tools appear, including the denied one.)

**Impact:** MCP clients present `tools/list` results to LLMs and users as the set of available capabilities. Denied tools appearing in the list:
1. Misleads the client — it will attempt to call `mock_echo__echo` and hit a hard error.
2. Defeats the purpose of `allowed_tools`/`deny_tools` at the discovery layer.
3. Could expose tool existence to callers who shouldn't know about it.

Policy must be applied at `tools/list` time, not only at `tools/call` time.

---

## HIGH

---

### BUG-02 — `forge add` has no HTTP or SSE transport flags

**Severity:** HIGH  
**Component:** CLI / forge add  
**Reproducible:** Always

**Steps to reproduce:**
```sh
forge add myserver --help
```

**Actual:**
```
Usage: forge add [OPTIONS] <NAME>
Options:
  --cmd <CMD>   Command to run (required for stdio transport)
```

**Impact:** `forge.toml` supports three transports — `stdio`, `http`, `sse`. The `forge add` command only supports `stdio`. To add an HTTP or SSE server, users must hand-edit `forge.toml`. There are no `--url` or `--transport` flags. This makes HTTP/SSE servers second-class citizens and is especially painful for remote MCP servers (the common cloud deployment case).

**Expected flags:**
```
--transport <TRANSPORT>  [default: stdio] [possible values: stdio, http, sse]
--url <URL>              Base URL for http/sse transport
--cmd <CMD>              Command for stdio transport
```

---

### BUG-03 — `forge add` accepts empty `--cmd ""`

**Severity:** HIGH  
**Component:** CLI / forge add / Input Validation  
**Reproducible:** Always

**Steps to reproduce:**
```sh
forge add emptyserver --cmd ""
forge check
```

**Actual:**
```
Added server 'emptyserver' to ./forge.toml   # Exit 0 — no error
# forge check then shows parse error on startup
```

**Expected:** `forge add` should validate that `--cmd` is non-empty before writing to `forge.toml`. Writing `cmd = ""` produces a config that passes `forge add` but fails at runtime — the validation gap means the error surfaces at startup, not at configuration time.

---

### BUG-06 — `forge status` is global and shows dead processes as running

**Severity:** HIGH  
**Component:** CLI / forge status / State Management  
**Reproducible:** Always when a previous session existed

**Steps to reproduce:**
```sh
# Create a project with a different forge.toml than a previous session
mkdir /tmp/new_project && cd /tmp/new_project
forge init --name testsvc --cmd /bin/echo
forge status
```

**Actual:**
```
NAME     STATUS    PID    UPTIME   RESTARTS  ENDPOINT   LAST_ERROR
github   running   65065  0s       0         -          -
```
(Shows a server from a completely different project. PID 65065 does not exist.)

**Expected:** `forge status` should show status for servers defined in the current directory's `forge.toml`, not a global state file. OR the state file should validate PID liveness before reporting "running".

**Root cause:** State is stored in `~/.forge/state.json` globally. No PID liveness check is performed. A dead process is forever "running" until explicitly stopped.

**Impact:** Operators cannot trust `forge status`. It reports wrong data in two ways: wrong project scope AND stale process state.

---

### BUG-10 — Parse errors return plain text, not JSON-RPC

**Severity:** HIGH  
**Component:** Proxy / HTTP / Protocol  
**Reproducible:** Always

**Steps to reproduce:**
```sh
curl -X POST http://127.0.0.1:3456/ -H "Content-Type: application/json" -d 'not-json'
```

**Actual:**
```
Failed to parse the request body as JSON: expected ident at line 1 column 2
```
(Plain text body, HTTP 400)

**Expected (JSON-RPC 2.0):**
```json
{"jsonrpc": "2.0", "error": {"code": -32700, "message": "Parse error"}, "id": null}
```
(HTTP 200 per JSON-RPC convention, or HTTP 400 with JSON body)

**Impact:** JSON-RPC clients expect all responses — including errors — to be JSON-RPC formatted. A plain text error response will crash parsers that try to deserialize every response as `JsonRpcResponse`. Same issue applies to the missing `Content-Type` rejection.

---

### BUG-11 — Auth rejection returns plain text, not JSON-RPC

**Severity:** HIGH  
**Component:** Proxy / Auth / Protocol  
**Reproducible:** Always when `auth_token` is configured

**Steps to reproduce:**
```sh
# proxy running with auth_token = "secret"
curl -X POST http://127.0.0.1:3458/ -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}'
```

**Actual:**
```
Unauthorized
```
(HTTP 401, plain text body)

**Expected:**
```json
{"jsonrpc": "2.0", "error": {"code": -32001, "message": "Unauthorized"}, "id": null}
```

**Impact:** Same as BUG-10 — clients expecting JSON-RPC will fail to parse the 401 response. Additionally, the `id: null` in the error response is important for client correlation. HTTP 401 status is correct; only the body format needs fixing.

---

### BUG-15 — Batch JSON-RPC requests not supported

**Severity:** HIGH  
**Component:** Proxy / Protocol  
**Reproducible:** Always

**Steps to reproduce:**
```sh
curl -X POST http://127.0.0.1:3456/ -H "Content-Type: application/json" \
  -d '[{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}},{"jsonrpc":"2.0","id":2,"method":"initialize","params":{...}}]'
```

**Actual:**
```
Failed to deserialize the JSON body into the target type: [0]: invalid type: map, expected a string at line 1 column 1
```

**Expected:** An array of JSON-RPC response objects (one per request in the batch).

**Impact:** JSON-RPC 2.0 specifies batch support. Some MCP clients (particularly those built on general-purpose JSON-RPC libraries) send batch requests for efficiency. Not supporting batches causes hard failures in compliant clients. The MCP Streamable HTTP transport spec does not explicitly exclude batch support.

---

### BUG-17 — `forge secret set` fails with "Device not configured"

**Severity:** HIGH  
**Component:** CLI / forge secret  
**Reproducible:** Always in non-interactive / piped input scenarios

**Steps to reproduce:**
```sh
echo "mypassword" | forge secret set my-secret
```

**Actual:**
```
Error: read password
Caused by: Device not configured (os error 6)
```

**Expected:** Secret stored in macOS Keychain under `mcp-forge/my-secret`.

**Root cause:** The command reads from `/dev/tty` for password input (to avoid piping), but `/dev/tty` is not available in non-interactive/CI contexts. The OS error 6 (`ENXIO`) is "No such device or address". Either the implementation should accept stdin when tty is unavailable, or provide a `--value` flag (with a clear security warning) for scripted use.

**Impact:** `forge secret set` is completely non-functional in any automated, CI, or scripted context. Even interactive terminal usage fails here with pipe input.

---

## MEDIUM

---

### BUG-05 — `forge check` missing `--config` flag

**Severity:** MEDIUM  
**Component:** CLI / forge check  
**Reproducible:** Always

**Steps to reproduce:**
```sh
forge check --config /path/to/other/forge.toml
```

**Actual:**
```
error: unexpected argument '--config' found
```

**Impact:** `forge start` has `--config <CONFIG>`. `forge check` does not. This breaks CI workflows where configs are validated before deployment. The `--config` flag exists in `forge start` but not in `forge check` — a direct inconsistency. Users validating non-local configs must `cd` to the config directory.

---

### BUG-07 — `forge stop` exposes internal file paths in error messages

**Severity:** MEDIUM  
**Component:** CLI / forge stop / Error UX  
**Reproducible:** Always when no daemon is running

**Steps to reproduce:**
```sh
forge stop
```

**Actual:**
```
Error: no ~/.forge/run.pid or daemon.pid found; forge does not appear to be running
```

**Expected:**
```
Error: no running forge proxy found. Start one with: forge start --daemon
```

**Impact:** Exposing `~/.forge/run.pid` is an implementation detail that should never appear in user-facing messages. It's confusing to non-technical users and reveals internal storage conventions. The error also doesn't guide the user toward a fix.

---

### BUG-08 — Cryptic error when configured server doesn't speak MCP

**Severity:** MEDIUM  
**Component:** CLI / forge start / Error UX  
**Reproducible:** When cmd is a non-MCP binary (e.g., `echo`, `cat`)

**Steps to reproduce:**
```toml
[server.myserver]
cmd = "echo"
transport = "stdio"
```
```sh
forge start
```

**Actual:**
```
Error: MCP handshake failed for 'myserver': Send message error Transport
[rmcp::transport::child_process::TokioChildProcess] error: Broken pipe (os error 32),
when send initialize request
```

**Expected:**
```
Error: server 'myserver': MCP handshake failed. 
The command 'echo' exited immediately without responding to MCP initialize.
Verify the command is a valid MCP server (run it manually to check).
```

**Impact:** New users frequently configure the wrong command. The current error leaks internal type names (`rmcp::transport::child_process::TokioChildProcess`) and provides no actionable guidance. Users don't know if this is a network issue, a configuration issue, or a bug in forge.

---

### BUG-16 — JSON-RPC notifications receive responses (protocol violation)

**Severity:** MEDIUM  
**Component:** Proxy / Protocol / JSON-RPC 2.0  
**Reproducible:** Always

**Steps to reproduce:**
```sh
# No "id" field = notification per JSON-RPC 2.0
curl -X POST http://127.0.0.1:3456/ -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","method":"tools/list","params":{}}'
```

**Actual:**
```json
{"jsonrpc":"2.0","result":{...},"error":null,"id":null}
```
(A response is returned with `"id": null`)

**Expected:** No response (HTTP 200 with empty body, or no body at all).

**JSON-RPC 2.0 spec (section 4):** *"A Notification is a Request object without an 'id' member. [...] The Server MUST NOT reply to a Notification."*

**Impact:** Clients that send notifications (fire-and-forget messages) will receive unexpected responses. This can cause protocol-level confusion in strict JSON-RPC 2.0 clients.

---

### BUG-19 — `forge logs` fails silently when not running in daemon mode

**Severity:** MEDIUM  
**Component:** CLI / forge logs  
**Reproducible:** Always when proxy was started without `--daemon`

**Steps to reproduce:**
```sh
forge start          # No --daemon flag
forge logs myserver
```

**Actual:**
```
Error: log file not found: /Users/prognosticator/.forge/logs/myserver.log
```

**Expected:** Either logs written regardless of daemon mode, OR a clear error:
```
Error: server 'myserver' has no log file. Logs are only written in daemon mode.
Restart with: forge start --daemon
```

**Impact:** The most common invocation of `forge start` (foreground mode, no daemon flag) writes no log files. When a user then runs `forge logs`, they get a confusing "not found" error with no indication that this is expected behavior for foreground mode. The `forge logs --help` output makes no mention of daemon mode.

---

## LOW

---

### BUG-04 — Generated `forge.toml` contains unnecessary default fields

**Severity:** LOW  
**Component:** CLI / forge init / forge add  
**Reproducible:** Always

**Example generated config:**
```toml
[server.myserver]
cmd = "echo hello"
transport = "stdio"
allowed_tools = []
deny_tools = []
max_calls_per_min = 60
tags = []

[server.myserver.secret]
[server.myserver.env]
```

**Expected (minimal form):**
```toml
[server.myserver]
cmd = "echo hello"
```

**Impact:** Default fields add noise, make diffs verbose, and can confuse users who don't know which fields are meaningful vs. redundant. Empty `[secret]` and `[env]` sections are especially distracting. Generated configs should emit only non-default values.

---

### BUG-13 — `forge report` displays excessive decimal precision in text output

**Severity:** LOW  
**Component:** CLI / forge report  
**Reproducible:** Always

**Actual:**
```
Server    Calls   Errors   Err%   Avg lat                     P99 lat   Est cost
mock_echo 135     10       7.4%   0.13333333333333333ms       2ms       $  0.00
```

**Expected:**
```
Server     Calls  Errors  Err%   Avg lat  P99 lat  Est cost
mock_echo  135    10      7.4%   0.13ms   2ms      $0.00
```

**Impact:** The text table renders `0.13333333333333333ms` — clearly a raw floating-point value with no formatting. The `$  0.00` also has spurious whitespace. Minor cosmetic issue but makes the report look unpolished.

---

### BUG-18 — `forge secret check` uses wrong server name in error

**Severity:** LOW  
**Component:** CLI / forge secret check  
**Reproducible:** Always when checking a missing env var

**Steps to reproduce:**
```sh
forge secret check env:NONEXISTENT_VAR
```

**Actual:**
```
Error: env var 'NONEXISTENT_VAR' not set (needed by server 'check')
```

**Expected:**
```
Error: env var 'NONEXISTENT_VAR' is not set
```

**Root cause:** The error message formatting code uses the subcommand name (`check`) as the server name string. `check` is the CLI subcommand, not a server.

---

### BUG-20 — Audit log latency shows 0ms for all real calls

**Severity:** LOW  
**Component:** Proxy / Audit  
**Reproducible:** Consistently observed; all 136 audit events show `lat=0ms` or `lat=2ms`

**Actual:**
```
forge audit --stats → avg_latency_ms=0.1
```

**Impact:** The audit latency field always shows `0ms` in the text display despite having real request/response round-trips. The `--stats` shows `avg_latency_ms=0.13` which proves latency IS being tracked, but the per-record text display rounds to the nearest ms (making values < 1ms display as 0). This renders the `lat=` column meaningless for in-process mock servers. The display should show sub-millisecond precision (e.g., `lat=0.13ms`).

---

## Positive Findings (Things That Work Correctly)

These were explicitly tested and confirmed working:

- `forge init` — creates valid `forge.toml`, correctly rejects re-init in existing project
- `forge add` — correctly rejects duplicate server names; writes valid TOML
- `forge check` — correctly validates server binary existence, port availability; catches empty-cmd at parse time
- `forge start` — starts proxy with real MCP server; handshakes correctly
- `tools/list` — returns correctly namespaced tools (`server__tool`)
- `tools/call` — routes and executes correctly; returns proper JSON-RPC responses
- `initialize` — correct MCP handshake response with protocol version and capabilities
- Rate limiting — fires exactly at configured threshold; correct error response format
- Injection detection — correctly blocks calls matching patterns; correct WARN log
- Auth token — correctly enforces Bearer token; HTTP 401 on invalid/missing token
- `deny_tools` call enforcement — blocks denied tools at call time with correct error code
- `forge audit` — records all events; `--errors` filter works; `--stats` works
- `forge status --json` — valid JSON output; `--watch` flag exists
- `forge report --format json` — valid JSON output with correct structure
- `forge secret check env:VAR` — correctly validates env var references
- `forge ls` — correctly lists configured servers
- `forge stop` / `forge start` — `forge init/add/stop` handle idempotency cases correctly

---

## Risk Assessment

| Area | Risk | Notes |
|------|------|-------|
| Security (auth) | HIGH | Auth returns plain text; clients may silently ignore rejections |
| Security (policy) | CRITICAL | Denied tools visible in `tools/list`; clients will attempt blocked calls |
| Protocol compliance | HIGH | No `/.well-known/mcp`, no batch, notification responses |
| Operator trust | HIGH | `forge status` shows stale/wrong data |
| Secret management | HIGH | `forge secret set` non-functional |
| CLI UX | MEDIUM | No `--version`, inconsistent `--config` flag, cryptic errors |

---

*Report generated from systematic manual QA: 2026-06-21*
