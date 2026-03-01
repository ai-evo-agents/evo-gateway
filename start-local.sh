#!/usr/bin/env bash
# Start evo-gateway from its own directory using the local build
set -euo pipefail
cd "$(dirname "$0")"
RUST_LOG=info GATEWAY_CONFIG=gateway.json exec ./target/release/evo-gateway "$@"
