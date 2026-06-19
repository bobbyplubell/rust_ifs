#!/usr/bin/env bash
# v3 end-to-end test: the v3 browser client against a real sheep-node.
#
# The node release binary serves the HTTP watch + write face on the HOST at
# 127.0.0.1:8080 (no node/npm on the host — this is a compiled Rust binary). The
# Playwright test runs in Docker with --network host so the container's loopback
# reaches that node. We reap the node + container on exit.
#
# Usage: ./e2e/run-v3.sh   (build the node first: cargo build --release -p sheep-node)
set -euo pipefail
cd "$(dirname "$0")/.."

PORT="${SHEEP_HTTP_PORT:-8080}"
NODE_BIN=target/release/sheep-node
DATA_DIR="$(mktemp -d /tmp/sheep-e2e-data.XXXXXX)"

if [ ! -x "$NODE_BIN" ]; then
  echo "building sheep-node (release)…"
  cargo build --release -p sheep-node
fi

# Start the node with a bootstrap flock so there is a live sheep to watch.
echo "==> starting sheep-node on 127.0.0.1:$PORT (bootstrap flock=2)"
SHEEP_BOOTSTRAP_FLOCK=2 "$NODE_BIN" \
  --http-addr "127.0.0.1:$PORT" --data-dir "$DATA_DIR" \
  >"$DATA_DIR/node.log" 2>&1 &
NODE_PID=$!

cleanup() {
  echo "==> cleanup"
  # Kill the node and any render threads it spawned, then reap.
  kill "$NODE_PID" 2>/dev/null || true
  wait "$NODE_PID" 2>/dev/null || true
  pkill -9 -f "$NODE_BIN --http-addr 127.0.0.1:$PORT" 2>/dev/null || true
  rm -rf "$DATA_DIR"
}
trap cleanup EXIT INT TERM

# Wait for the watch face to come up (a live sheep takes a moment to mint).
echo "==> waiting for the node to serve a live flock"
for i in $(seq 1 30); do
  if curl -sf "http://127.0.0.1:$PORT/health" | grep -q '"live_flock":[1-9]'; then
    echo "    node ready: $(curl -s http://127.0.0.1:$PORT/health)"
    break
  fi
  sleep 1
  if [ "$i" -eq 30 ]; then echo "node never served a live flock"; cat "$DATA_DIR/node.log"; exit 1; fi
done

# Run the Playwright test in Docker, sharing the host network so 127.0.0.1:$PORT
# inside the container is the host node. No node/npm on the host.
echo "==> running Playwright (docker, --network host)"
docker run --rm --network host -v "$PWD":/repo:z -w /repo/e2e \
  -e NODE_URL="http://127.0.0.1:$PORT" \
  mcr.microsoft.com/playwright:v1.53.0-noble \
  bash -c 'npm install --no-audit --no-fund >/dev/null 2>&1; node v3.js'
