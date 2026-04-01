#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "Usage: $0 vX.Y.Z"
  exit 1
fi

tag="$1"
if [[ ! "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "Invalid tag format: $tag (expected vX.Y.Z)"
  exit 1
fi

branch="$(git rev-parse --abbrev-ref HEAD)"
if [[ "$branch" != "development" ]]; then
  echo "Current branch is '$branch'. Switch to 'development' before releasing."
  exit 1
fi

if [[ -n "$(git status --porcelain)" ]]; then
  echo "Working tree is not clean. Commit or stash changes first."
  exit 1
fi

echo "Running release validation gates..."
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all
cargo build --workspace --release

echo "Pushing development..."
git push origin development

echo "Creating tag $tag..."
git tag "$tag"

echo "Pushing tag $tag..."
git push origin "$tag"

echo "Release tag pushed: $tag"
echo "Watch GitHub Actions release workflow for publish status."
