# MCP Forge

A Rust workspace for `mcp-forge`, a CLI and daemon for managing MCP servers and tool execution.

> See `PLAN.md` for the full design, architecture, timeline, risk register, testing strategy, and release plan.

## What this repository contains

- `crates/forge-cli` — CLI binary powered by `clap`.
- `crates/forge-core` — core logic for config, supervisor, audit, secret resolution, and protocol handling.
- `crates/forge-proxy` — HTTP/axum proxy layer for MCP traffic and API exposure.
- `crates/forge-mock-mcp` — mock MCP server for local integration testing.
- `tests/` — workspace-level integration tests.
- `forge.toml.example` — example runtime configuration.
- `PLAN.md` — detailed development plan and architecture review.

## Project goals

- Provide a unified management layer for MCP servers.
- Offer a stable CLI interface for start/stop/status/audit operations.
- Support secure secret resolution and transparent tool invocation.
- Keep the architecture modular and easy to extend.

## Getting started

### Prerequisites

- Rust toolchain (stable)
- `cargo` installed

### Build

```bash
cargo build --workspace
```

### Run tests

```bash
cargo test --all
```

### Lint

```bash
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
```

## Workspace layout

The repository is organized as a Cargo workspace. The root `Cargo.toml` defines shared dependencies and members.

### Crate roles

- `forge-cli`: Entrypoint CLI and command dispatch.
- `forge-core`: Shared domain models, supervisor, config, and audit modules.
- `forge-proxy`: HTTP proxy and API server implementation.
- `forge-mock-mcp`: Local mock MCP implementation for testing.

## Recommended workflow

1. Read `PLAN.md` first to understand the architecture and execution plan.
2. Implement new features in `crates/forge-core` where core behavior belongs.
3. Wire CLI commands in `crates/forge-cli`.
4. Add integration coverage in `tests/`.

## CI

The repository uses GitHub Actions at `.github/workflows/ci.yml` to run:

- `cargo fmt --all -- --check`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test --all`

## Contributing

- Follow the workspace conventions.
- Keep implementations small and modular.
- Add tests for new behavior at the crate and workspace level.
- Update `PLAN.md` if the architecture or design changes significantly.

## License

MIT
