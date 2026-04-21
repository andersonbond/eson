#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export ESON_WORKSPACE_ROOT="${ESON_WORKSPACE_ROOT:-$ROOT/workspace}"
export ESON_MEMORY_PORT="${ESON_MEMORY_PORT:-8888}"
export ESON_AGENT_HTTP_PORT="${ESON_AGENT_HTTP_PORT:-8787}"

echo "Starting eson-memory on :$ESON_MEMORY_PORT …"
cargo run -p eson-memory --manifest-path "$ROOT/Cargo.toml" &
MEM_PID=$!
sleep 1

cleanup() {
  kill "$MEM_PID" 2>/dev/null || true
}
trap cleanup EXIT

echo "Starting eson-agent on :$ESON_AGENT_HTTP_PORT …"
cargo run -p eson-agent --manifest-path "$ROOT/Cargo.toml"
