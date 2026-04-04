# Contributing

Thanks for taking the time to contribute to MCP Forge.

## Prerequisites

- Rust stable toolchain — install via [rustup](https://rustup.rs)
- `cargo` (included with Rust)
- Node.js (optional, for running real MCP server examples in tests)

## Development Setup

```bash
git clone https://github.com/devesrawat/MCPForge.git
cd MCPForge
cargo build --workspace
```

## Running Tests

```bash
cargo test --all
```

The workspace includes a mock MCP server (`forge-mock-mcp`) used by integration tests. Build it first if tests fail to locate it:

```bash
cargo build -p forge-mock-mcp
```

## Linting

All PRs must pass:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
```

Run `cargo fmt --all` to auto-format before committing.

## Making Changes

1. Fork the repo and create a feature branch:
   ```bash
   git checkout -b feat/your-feature
   ```
2. Write tests before or alongside your changes (`cargo test --all` must stay green).
3. Keep `fmt` and `clippy` clean.
4. Commit using [conventional commits](https://www.conventionalcommits.org/):
   ```
   feat: add per-server timeout config
   fix: correct rate limit window reset
   docs: update forge.toml.example
   ```
5. Push your branch and open a pull request against `development`.

## Pull Request Guidelines

- Include a clear description of what the change does and why.
- Link related issues if applicable.
- Small, focused PRs are preferred over large omnibus changes.
- Tests are required for bug fixes and new features.

## Reporting Issues

Use the GitHub issue tracker. For bugs, include:

- Your OS and Rust version (`rustc --version`)
- The `forge` version (`forge --version`)
- Relevant `forge.toml` config (redact secrets)
- Full error output
