#!/bin/bash
set -euo pipefail

# Build and install evo-gateway binaries to system PATH.
# Symlinks release binaries to the Homebrew bin directory (or /usr/local/bin).

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

echo "Building evo-gateway (release)..."
cargo build --release

# Determine install directory
if command -v brew &>/dev/null; then
    BIN_DIR="$(brew --prefix)/bin"
else
    BIN_DIR="/usr/local/bin"
fi

echo "Linking binaries to ${BIN_DIR}..."
ln -sf "${SCRIPT_DIR}/target/release/evo-gateway" "${BIN_DIR}/evo-gateway"
ln -sf "${SCRIPT_DIR}/target/release/evo-gateway-cli" "${BIN_DIR}/evo-gateway-cli"

echo ""
echo "Installed:"
echo "  evo-gateway     -> $(which evo-gateway 2>/dev/null || echo "${BIN_DIR}/evo-gateway")"
echo "  evo-gateway-cli -> $(which evo-gateway-cli 2>/dev/null || echo "${BIN_DIR}/evo-gateway-cli")"
echo ""
echo "Commands:"
echo "  evo-gateway                              # start server"
echo "  evo-gateway auth cursor                  # authenticate cursor"
echo "  evo-gateway auth claude                  # authenticate claude code"
echo "  evo-gateway auth codex                   # authenticate codex CLI"
echo "  evo-gateway service install              # install as launchd/systemd service"
echo "  evo-gateway service start                # start the service"
echo "  evo-gateway service status               # check service status"
echo ""
echo "  evo-gateway-cli auth generate --name <n> # generate API key"
echo "  evo-gateway-cli auth list                # list API keys"
echo "  evo-gateway-cli auth revoke --name <n>   # revoke API key"
