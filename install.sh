#!/usr/bin/env bash
set -euo pipefail

REPO="devesrawat/MCPForge"
BINARY_NAME="forge"
TAG="${1:-latest}"

os="$(uname -s | tr '[:upper:]' '[:lower:]')"
arch="$(uname -m)"

case "$os" in
  linux) os_target="unknown-linux-musl" ;;
  darwin) os_target="apple-darwin" ;;
  *)
    echo "Unsupported OS: $os"
    exit 1
    ;;
esac

case "$arch" in
  x86_64|amd64) arch_target="x86_64" ;;
  arm64|aarch64) arch_target="aarch64" ;;
  *)
    echo "Unsupported architecture: $arch"
    exit 1
    ;;
esac

triple="${arch_target}-${os_target}"

if [[ "$TAG" == "latest" ]]; then
  asset_url="https://github.com/${REPO}/releases/latest/download/${BINARY_NAME}-${triple}.tar.gz"
else
  asset_url="https://github.com/${REPO}/releases/download/${TAG}/${BINARY_NAME}-${triple}.tar.gz"
fi

checksum_url="${asset_url}.sha256"

tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

archive_path="$tmp_dir/${BINARY_NAME}.tar.gz"
curl -fsSL "$asset_url" -o "$archive_path"

checksum_path="$tmp_dir/${BINARY_NAME}.sha256"
curl -fsSL "$checksum_url" -o "$checksum_path"

if command -v shasum >/dev/null 2>&1; then
  expected="$(awk '{print $1}' "$checksum_path")"
  actual="$(shasum -a 256 "$archive_path" | awk '{print $1}')"
elif command -v sha256sum >/dev/null 2>&1; then
  expected="$(awk '{print $1}' "$checksum_path")"
  actual="$(sha256sum "$archive_path" | awk '{print $1}')"
else
  echo "No SHA-256 tool found (shasum/sha256sum)."
  exit 1
fi

if [[ "$expected" != "$actual" ]]; then
  echo "Checksum verification failed for downloaded archive."
  exit 1
fi

tar -xzf "$archive_path" -C "$tmp_dir"

if [[ ! -f "$tmp_dir/$BINARY_NAME" ]]; then
  echo "Downloaded archive did not contain '$BINARY_NAME'."
  echo "Check release assets for naming differences."
  exit 1
fi

install_dir="${HOME}/.local/bin"
mkdir -p "$install_dir"
install -m 0755 "$tmp_dir/$BINARY_NAME" "$install_dir/$BINARY_NAME"

echo "Installed $BINARY_NAME to $install_dir/$BINARY_NAME"
if [[ ":$PATH:" != *":$install_dir:"* ]]; then
  echo "Add to PATH: export PATH=\"$install_dir:\$PATH\""
fi
