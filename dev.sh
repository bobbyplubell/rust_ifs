#!/usr/bin/env bash
# Dev runner for v2: the coordinator (API) + a static server for the client.
# Open the printed client URL in TWO browser contexts (e.g. a normal window and
# an incognito window = two different auto-generated keys) to test with two
# clients contributing to the same flock.
#
#   ./dev.sh            # one world, GEN_MS=5min
#   GEN_MS=60000 ./dev.sh   # faster generations for quick testing
set -euo pipefail
cd "$(dirname "$0")"

DATA_DIR="${DATA_DIR:-/tmp/coord-dev}"
COORD_PORT="${COORD_PORT:-8080}"
WEB_PORT="${WEB_PORT:-8000}"
mkdir -p "$DATA_DIR"

echo "building + starting coordinator…"
BIND="127.0.0.1:${COORD_PORT}" DATA_DIR="$DATA_DIR" \
  GENOMES_DIR="$PWD/web/genomes" GEN_MS="${GEN_MS:-300000}" \
  cargo run -p coordinator &
COORD=$!

# Serve the client. config.js must point COORDINATOR at http://localhost:COORD_PORT
# (its default). CORS on the coordinator is allow-any, so cross-port is fine.
( cd web && exec python3 -m http.server "$WEB_PORT" ) &
WEB=$!

trap 'kill "$COORD" "$WEB" 2>/dev/null || true' EXIT INT TERM

cat <<MSG

  ──────────────────────────────────────────────
  coordinator API : http://localhost:${COORD_PORT}/api/flock
  client          : http://localhost:${WEB_PORT}/

  → open the client in TWO browser contexts to test two clients:
      • a normal window  +  an incognito/private window
      • or two different browsers
    each gets its own auto-generated key = a distinct identity.
    Pledge/contribute in both; watch the flock + credits update.

  Ctrl-C to stop.
  ──────────────────────────────────────────────
MSG

wait
