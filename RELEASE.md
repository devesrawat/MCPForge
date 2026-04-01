# Release Runbook

This file defines a repeatable release process for mcp-forge.

## Scope

- v0.1.0: MVS release (proxy + namespacing + limits)
- v1.0.0: production release

## Preconditions

1. You are on the `development` branch with a clean working tree.
2. CI is green on the latest commit.
3. GitHub release workflow exists at `.github/workflows/release.yml`.
4. Homebrew tap repository is ready: `devesrawat/homebrew-mcp-forge`.

## Local Validation Gate (must pass)

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all
cargo build --workspace --release
```

## Version Bump

Update crate versions in:

- `Cargo.toml` (workspace root)
- `crates/forge-core/Cargo.toml`
- `crates/forge-proxy/Cargo.toml`
- `crates/forge-cli/Cargo.toml`
- `crates/forge-mock-mcp/Cargo.toml`

Commit:

```bash
git add Cargo.toml crates/*/Cargo.toml
git commit -m "chore: bump version to X.Y.Z"
```

## Tag and Publish

Preferred one-command path:

```bash
./scripts/release-tag.sh vX.Y.Z
```

Manual path:

```bash
git push origin development
git tag vX.Y.Z
git push origin vX.Y.Z
```

This triggers `.github/workflows/release.yml` and publishes release artifacts.

## Post-Release Checks

1. Verify GitHub release exists with all artifacts.
2. Verify checksums exist for each artifact.
3. Verify installer works:

```bash
./install.sh vX.Y.Z
forge --help
```

1. Verify Homebrew install (after tap update):

```bash
brew tap devesrawat/mcp-forge
brew install forge
forge --help
```

## Rollback

If release workflow fails before publish completion:

```bash
git tag -d vX.Y.Z
git push --delete origin vX.Y.Z
```

If GitHub release was created but is bad:

1. Mark release as draft or delete it in GitHub.
2. Delete remote tag and local tag (commands above).
3. Fix issue in `development`.
4. Re-tag with next patch version (`vX.Y.(Z+1)`).

## Real-Life Examples

### Example 1: First public release (`v0.1.0`)

You have merged the MVS scope to `development`, CI is green, and you want to publish the first usable binary.

```bash
# 1) run local gate
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all
cargo build --workspace --release

# 2) bump versions in Cargo.toml files to 0.1.0
git add Cargo.toml crates/*/Cargo.toml
git commit -m "chore: bump version to 0.1.0"

# 3) create and push tag
./scripts/release-tag.sh v0.1.0
```

After workflow completion, validate install paths:

```bash
./install.sh v0.1.0
forge --help
```

### Example 2: Patch hotfix (`v0.1.1`)

You found a production bug in namespaced routing and need a fast patch release.

```bash
# 1) fix bug on development and merge
# 2) run full validation gate
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all

# 3) bump patch version
git add Cargo.toml crates/*/Cargo.toml
git commit -m "fix: release 0.1.1 hotfix"

# 4) tag and publish
./scripts/release-tag.sh v0.1.1
```

Then verify `forge status` and one `tools/list` call against your MCP client before announcing.

### Example 3: Failed tag publish and recovery

The release workflow fails after tag push due to a packaging issue.

```bash
# remove bad tag locally and remotely
git tag -d v0.1.2
git push --delete origin v0.1.2
```

Next steps:

1. Fix the packaging issue on `development`.
2. Re-run local validation gate.
3. Re-tag using the next patch (`v0.1.3`) instead of reusing `v0.1.2`.

## Milestone Checklists

### v0.1.0 checklist

- [ ] `forge start` spawns servers and starts proxy
- [ ] `tools/list` returns namespaced tools
- [ ] `tools/call` routes correctly
- [ ] rate limit error path validated
- [ ] `forge stop` cleans up
- [ ] quickstart docs validated

### v1.0.0 checklist

- [ ] cross-platform artifacts available
- [ ] installer (`install.sh`) validated
- [ ] Homebrew install validated
- [ ] full test suite green
- [ ] CHANGELOG/release notes prepared
- [ ] external announcements prepared
