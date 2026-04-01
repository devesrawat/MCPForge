# mcp-forge Validation Report

**Date:** 2026-03-31
**Branch:** development
**Commit:** 95312d7
**Validator:** Claude Sonnet 4.6

---

## Executive Summary

| Metric | Result |
|--------|--------|
| Build | **PASS** — zero errors, zero warnings |
| Clippy (correctness + suspicious + perf) | **PASS** — zero diagnostics |
| Tests | **37/37 PASS** |
| Test ratio (lines of tests / production) | ~4.7% — **below 80% target** |
| Critical bugs | 2 |
| High bugs | 3 |
| Medium issues | 7 |

---

## 1. Build & Lint

```
cargo build        → Finished in 4.74s — 0 errors, 0 warnings
cargo clippy --all → Finished in 6.27s — 0 diagnostics
```

All four workspace members compile cleanly: `forge-core`, `forge-cli`, `forge-proxy`, `forge-mock-mcp`.

---

## 2. Test Results

### 2.1 Pass/Fail by Suite

| Suite | Location | Tests | Result |
|-------|----------|-------|--------|
| forge-core unit | `src/lib.rs` | 10 | **PASS** |
| injection detection | `tests/injection_detection.rs` | 13 | **PASS** |
| config parsing | `tests/config_parsing.rs` | 5 | **PASS** |
| secret resolution | `tests/secret_resolution.rs` | 1 | **PASS** |
| supervisor | `tests/supervisor.rs` | 1 | **PASS** |
| forge-proxy unit | `src/lib.rs` | 2 | **PASS** |
| security hardening | `tests/security_hardening.rs` | 5 | **PASS** |
| forge-cli | — | 0 | no tests |
| forge-mock-mcp | — | 0 | no tests |
| **Total** | | **37** | **37 PASS / 0 FAIL** |

### 2.2 Notes on Deleted Tests

Two root-level test files are listed as deleted in `git status`:

- `tests/config_parsing.rs`
- `tests/secret_resolution.rs`

These tests **still run and pass** — they appear to have been moved rather than deleted (the build system still finds them). Verify the git status is a rename, not an accidental deletion.

---

## 3. New Code — This PR

Three new artifacts were added in this batch of changes:

| File | Lines | Purpose |
|------|-------|---------|
| `crates/forge-cli/src/commands/check.rs` | 146 | `forge check` — validates config, secrets, glob patterns |
| `crates/forge-core/src/injection.rs` | 192 | Prompt injection detector (scan args + results) |
| `crates/forge-core/tests/injection_detection.rs` | 109 | Integration tests for injection detector |
| `crates/forge-proxy/tests/security_hardening.rs` | 63 | Structural security validation |

Integration points:

- `forge-core/src/lib.rs` — exports `injection` module
- `forge-cli/src/commands/mod.rs` and `main.rs` — registers `check` subcommand
- `forge-proxy/src/lib.rs` — wires `InjectionDetector` into `ProxyAppState` and `tools/call` dispatch

---

## 4. Bugs & Issues

### 4.1 Critical

**C1 — `check.rs`: dead variables (`allow_checked`, `deny_checked`)**
File: `crates/forge-cli/src/commands/check.rs` ~lines 96–175
Variables are incremented but never read. The check reports `[OK] glob patterns` unconditionally even when patterns are invalid. Error count is incremented but success message is still printed.
**Impact:** `forge check` lies about glob pattern validity — users get a false green.
**Fix:** Gate the success message on `allow_checked > 0 && errors == 0`.

**C2 — `CostGuard`: atomic race + off-by-one**
File: `crates/forge-proxy/src/lib.rs`, `CostGuard` struct
The day-rollover path uses `Ordering::Relaxed` for the load then `SeqCst` for the compare-exchange — inconsistent ordering across threads. Separately, `fetch_add` is called **before** checking the limit, so the counter increments past the cap on the boundary call.
**Impact:** Daily cost limit can be exceeded by one call per thread per rollover; under concurrent load multiple threads may double-insert the rollover.
**Fix:** Use `Ordering::SeqCst` consistently; check limit before incrementing (or decrement on rejection).

### 4.2 High

**H1 — `supervisor/mod.rs`: `child.id()` masked with `unwrap_or(0)`**
File: `crates/forge-core/src/supervisor/mod.rs` ~line 251
A PID of 0 is written to `state.json` silently when `child.id()` returns `None`. Downstream tooling (`forge status`, `forge stop` via SIGTERM) will operate on PID 0, which on Unix sends signals to the **entire process group**.
**Impact:** `forge stop` could SIGTERM unrelated processes.
**Fix:** Return an error if PID is unavailable; do not fall back to 0.

**H2 — `supervisor/mod.rs`: stderr capture errors break silently**
File: `crates/forge-core/src/supervisor/mod.rs` ~lines 345–351
A read error on the child's stderr stream exits the capture loop without any log output or cleanup signal. The supervisor continues thinking the server is healthy.
**Impact:** Lost log lines; supervisor may not detect a broken child.
**Fix:** Log the error at `warn!` level, then break explicitly.

**H3 — `logs.rs`: `read_to_string` on seeked file**
File: `crates/forge-cli/src/commands/logs.rs` ~line 45
After seeking to the tail position, `read_to_string` accumulates the entire remainder into a `String` on every poll iteration in `--follow` mode. For active, growing logs this allocates unboundedly.
**Impact:** Memory growth + incorrect line counting for large log files.
**Fix:** Use `BufReader::lines()` on the seeked file.

### 4.3 Medium

**M1 — Injection regex: no word boundaries**
File: `crates/forge-core/src/injection.rs`
Patterns like `ignore all previous instructions` will match inside longer words (e.g., `"ignorepreviousinstructions"` in a URL). No `\b` word boundary anchors.
**Risk:** False positives on benign tool arguments.

**M2 — Injection detector always in `Warn` mode in proxy**
File: `crates/forge-proxy/src/lib.rs`, `ProxyAppState::new()`
`InjectionDetector` is hard-coded to `InjectionMode::Warn` — direct injection is logged but the call is never blocked. The `Block` mode exists but is unreachable from the proxy path.
**Risk:** Injection protection is cosmetic; users who expect blocking get only logging.

**M3 — Audit write errors silently dropped**
File: `crates/forge-proxy/src/lib.rs` ~line where `audit_writer.record()` is called
The result of `audit_writer.record(event)` is not checked. Channel saturation or thread panic will silently lose audit records.
**Risk:** Compliance/audit trail gaps with no operator visibility.

**M4 — `init.rs`: no server name validation**
File: `crates/forge-cli/src/commands/init.rs`
Server names accept arbitrary Unicode including spaces, slashes, and shell metacharacters. These are later used to derive file paths (`~/.forge/logs/{name}.log`, `~/.forge/stopped/{name}`).
**Risk:** Path traversal on crafted server names; broken shell integrations.

**M5 — Audit DB: no schema versioning**
File: `crates/forge-core/src/audit.rs`
`CREATE TABLE IF NOT EXISTS` runs without a schema version table. Future column additions will silently fail against existing databases.
**Risk:** Silent data loss or panics on upgrade.

**M6 — Tool cache: stale after server restart**
File: `crates/forge-core/src/mcp.rs`, `ToolRegistry`
Cache TTL is per-entry wall-clock time. If a server restarts and exposes different tools, the old list remains valid for up to `FORGE_TOOL_CACHE_TTL_SECS` (default 300 s).
**Risk:** Proxy routes calls to tools that no longer exist on the restarted server.

**M7 — `supervisor.rs` test: unsafe `env::set_var`**
File: `crates/forge-core/tests/supervisor.rs`
`std::env::set_var("HOME", ...)` is called without synchronization inside a test. Rust's test harness runs tests in parallel by default.
**Risk:** Flaky test failures; undefined behavior in Rust's `unsafe` env API.

---

## 5. Test Coverage Gaps

The overall test-to-production ratio is ~4.7%. Areas with **zero** test coverage:

| Area | Risk |
|------|------|
| `forge check` command | C1 bug above is undetectable |
| Rate limiter enforcement | Governor exhaustion untested |
| Cost guard (day rollover, concurrency) | C2 bug above |
| RBAC policy enforcement (block path) | Policy bypass risk |
| Proxy injection blocking path | M2 bug above |
| Daemon spawn & PID file management | H1 risk |
| Secret resolution (keychain path) | Regression risk |
| HTTP transport variant | Entire transport unimplemented |
| Concurrent tool calls | Race conditions |

**Minimum recommended additions before MVS:**

1. `forge check` — test with a config containing a bad glob and a missing secret env var; assert exit code 1.
2. `CostGuard` — unit test day rollover at the exact limit, concurrent requests.
3. Proxy injection — integration test with `InjectionMode::Block` that asserts `-32000` error returned.
4. RBAC deny — proxy test asserting `PolicyDenied` for a tool on the deny list.

---

## 6. Architecture Observations

**Strengths:**

- Clean workspace split (cli / core / proxy) with low coupling.
- Secret management through OS keychain (no plaintext in config).
- RBAC + rate limiting + cost guard all wired together in a single dispatch path.
- Injection detector recurses through arbitrary JSON — covers indirect injection in tool results.
- Supervisor exponential backoff is well-structured.

**Risks before MVS (week 3):**

- HTTP transport is declared in `Transport` enum but not implemented in `McpTransport`.
- `GuardConfig.enabled` field exists but is read nowhere — guard can never be disabled via config.
- No request authentication on the proxy HTTP listener; any local process can call it.

---

## 7. Verdict

| Category | Status |
|----------|--------|
| Build | **GREEN** |
| Tests | **GREEN** (37/37) |
| Correctness | **YELLOW** — 2 critical, 3 high bugs |
| Test coverage | **RED** — 4.7%, target 80% |
| Clippy | **GREEN** |

**Recommendation:** Fix C1 and C2 before merging to `main`. H1 (PID 0 hazard) should also be resolved before the proxy is exposed to any real workload. Coverage gaps are acceptable for the current development phase but should be tracked.
