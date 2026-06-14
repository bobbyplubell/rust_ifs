#!/usr/bin/env bash
# P2P CONNECTIVITY INTEGRATION TEST — catches the recurring "0 peers / clear your
# browser data" regressions BEFORE deploy. One command: builds, runs the real web
# app in headless Chromium against a real relay behind Caddy (wss), asserts four
# properties, tears down, and exits 0 (pass) / 1 (fail).
#
#   (a) CONNECT     each peer reaches getSubscribers('sheep/v2')>0 within ~30s
#   (b) MESH        the relay's [stat] log shows mesh>0 (guards the peer-score
#                   regression that left subscribers>0 but mesh=0 => 0 peers)
#   (c) SYNC        a late joiner converges to an established peer's facts
#   (d) REGRESSION  a returning client with a PRE-POPULATED IndexedDB store still
#                   connects fast (guards commit 30e4526: net.start() before the
#                   heavy genesis→now flock replay)
#
# No Node on the host — everything runs in docker. Usage:  ./e2e/connectivity/run.sh
set -euo pipefail
cd "$(dirname "$0")"

PW_IMAGE="mcr.microsoft.com/playwright:v1.53.0-noble"
NODE_IMAGE="node:22"
PROJECT="sheepconn"
COMPOSE=(docker compose -p "$PROJECT" -f compose.yml)
CONNECT_PEERS="${CONNECT_PEERS:-3}"
FAIL=0

cleanup() {
  echo "==> tearing down"
  docker ps -aq --filter "name=${PROJECT}-" | xargs -r docker rm -f >/dev/null 2>&1 || true
  "${COMPOSE[@]}" down -v --remove-orphans >/dev/null 2>&1 || true
}
trap cleanup EXIT

# --- 0. prerequisites: libp2p browser bundle + playwright npm package ---------
# The app imports web/js/vendor/libp2p.js; build it (in docker) if missing. The
# bundle carries the BROWSER side of the connectivity logic, so to guard a change
# to relay/src/browser-entry.js, rebuild it: REBUILD_BUNDLE=1 ./run.sh
if [ ! -f ../../web/js/vendor/libp2p.js ] || [ -n "${REBUILD_BUNDLE:-}" ]; then
  echo "==> building libp2p browser bundle"
  (cd ../../relay && ./build.sh)
fi
# Peers mount this dir; ensure the playwright npm package is resolvable.
if [ ! -d node_modules/playwright ]; then
  echo "==> installing playwright npm package (one-off, in docker)"
  docker run --rm -v "$PWD":/w:z -w /w "$NODE_IMAGE" \
    npm install --no-audit --no-fund >/dev/null
fi

# --- 1. start relay + Caddy, scrape the relay's peer id -----------------------
echo "==> starting relay + swarm (Caddy)"
"${COMPOSE[@]}" up -d --build relay swarm
for i in $(seq 1 30); do
  PID=$("${COMPOSE[@]}" logs relay 2>/dev/null | grep -oE '12D3KooW[0-9A-Za-z]+' | head -1) \
    && [ -n "$PID" ] && break
  sleep 2
done
[ -n "${PID:-}" ] || { echo "FAIL: no relay peer id"; exit 1; }
RELAY_MADDR="/dns4/swarm/tcp/443/tls/ws/p2p/$PID"
echo "==> relay $RELAY_MADDR"

# Helper: run one peer role as a one-shot container ON THE COMPOSE NETWORK so it
# can reach the `swarm` (Caddy) and `relay` services by name. We use plain
# `docker run --network` rather than `compose run` because the peer image
# (Playwright) is not a compose service. Each container is NAMED ${PROJECT}-<name>
# so a long-lived one (e.g. a beacon) can be stopped explicitly — killing the host
# `docker run` process alone does NOT stop the container, which would leave the
# orchestrator's `wait` hanging.
# Args: <role> <name> [extra KEY=VAL env]...  -> exit code is the peer's.
NET="${PROJECT}_default"
peer() {
  local role="$1" name="$2"; shift 2
  local envs=(-e "ROLE=$role" -e "PEER=$name" -e "WEB_URL=https://swarm"
    -e "RELAY_MADDR=$RELAY_MADDR" -e "CONNECT_MS=${CONNECT_MS:-30000}"
    -e "SYNC_MS=${SYNC_MS:-45000}")
  for kv in "$@"; do envs+=(-e "$kv"); done
  docker run --rm --name "${PROJECT}-${name}" --network "$NET" "${envs[@]}" \
    -v "$PWD:/work" -w /work "$PW_IMAGE" \
    bash -lc "node peer.mjs"
}
# Stop a named peer's CONTAINER (and reap the backgrounded `docker run`).
stop_peer() { docker rm -f "${PROJECT}-$1" >/dev/null 2>&1 || true; wait "${2:-}" 2>/dev/null || true; }

# --- 2. CONNECT + MESH --------------------------------------------------------
# Launch N connect-peers (they auto-contribute => join the gossip mesh). Each
# asserts getSubscribers>0 itself; we then read the relay log for mesh>0.
echo "==> (a/b) CONNECT + MESH: $CONNECT_PEERS peers"
pids=()
for i in $(seq 1 "$CONNECT_PEERS"); do
  peer connect "conn$i" "HOLD_MS=45000" > "/tmp/conn$i.log" 2>&1 &
  pids[$i]=$!
done

# While they hold their subscriptions, sample the relay's [stat] mesh value.
mesh_seen=0
for s in $(seq 1 24); do
  sleep 2
  m=$("${COMPOSE[@]}" logs relay 2>/dev/null \
      | grep -oE 'mesh=[0-9]+' | tail -1 | grep -oE '[0-9]+' || echo 0)
  if [ "${m:-0}" -gt 0 ]; then mesh_seen="$m"; fi
done

for i in $(seq 1 "$CONNECT_PEERS"); do
  if wait "${pids[$i]}"; then echo "  conn$i: PASS"; else echo "  conn$i: FAIL"; FAIL=1; fi
  sed 's/^/    /' "/tmp/conn$i.log" | grep -E "CONNECT ok|FAIL|pageerror|ERROR" || true
done

if [ "$mesh_seen" -gt 0 ]; then
  echo "  MESH: PASS (relay reported mesh=$mesh_seen)"
else
  echo "  MESH: FAIL — relay never reported mesh>0 (peer-score regression?)"
  "${COMPOSE[@]}" logs relay 2>/dev/null | grep -E '\[stat\]' | tail -5 | sed 's/^/    /'
  FAIL=1
fi

# --- 3. SYNC (anti-entropy late joiner) --------------------------------------
echo "==> (c) SYNC: producer establishes facts, late joiner converges"
peer producer prod > /tmp/prod.log 2>&1 &
PROD_PID=$!
# Wait for the producer to print its target sheep + batch count.
TARGET=""; WANT=""
for s in $(seq 1 40); do
  line=$(grep -E 'PRODUCER ready' /tmp/prod.log | tail -1 || true)
  if [ -n "$line" ]; then
    TARGET=$(echo "$line" | grep -oE 'target=[0-9a-f]+' | cut -d= -f2)
    WANT=$(echo "$line" | grep -oE 'count=[0-9]+' | cut -d= -f2)
    break
  fi
  sleep 2
done
if [ -z "$TARGET" ] || [ -z "$WANT" ] || [ "$WANT" -eq 0 ]; then
  echo "  SYNC: FAIL — producer never published a batch set"; sed 's/^/    /' /tmp/prod.log; FAIL=1
else
  echo "  producer target=${TARGET:0:8} count=$WANT — starting late joiner"
  if peer latejoiner late "TARGET_SHEEP=$TARGET" "WANT_COUNT=$WANT" > /tmp/late.log 2>&1; then
    echo "  SYNC: PASS"; grep -E "SYNC ok" /tmp/late.log | sed 's/^/    /'
  else
    echo "  SYNC: FAIL"; sed 's/^/    /' /tmp/late.log; FAIL=1
  fi
fi
stop_peer prod "$PROD_PID"

# --- 4. REGRESSION (commit 30e4526: net.start() BEFORE the flock replay) ------
echo "==> (d) REGRESSION: returning client with a heavy IndexedDB store"

# (d1) DETERMINISTIC SOURCE GUARD — the actual shape of commit 30e4526 is an
# ORDERING invariant in main(): net.start() must be called BEFORE rebuildFlock().
# In a clean synthetic env the replay can't be made reliably 30s+ to catch this by
# a wall-clock race, and getSubscribers/recv timing is starved by the very replay
# we're testing — so guard the invariant where it's unambiguous: in the source.
# This can NEVER silently regress and pins the fix exactly.
APP="../../web/js/app.js"
order=$(grep -nE 'await (net\.start|rebuildFlock)\(\)' "$APP" \
  | grep -oE 'net\.start|rebuildFlock' | head -2 | tr '\n' ',')
if [ "$order" = "net.start,rebuildFlock," ]; then
  echo "  ORDERING: PASS (app.js calls net.start() before rebuildFlock())"
else
  echo "  ORDERING: FAIL — app.js main() must call net.start() BEFORE rebuildFlock()"
  echo "    found order: [$order] (the commit-30e4526 regression)"
  grep -nE 'await (net\.start|rebuildFlock)\(\)' "$APP" | sed 's/^/    /'
  FAIL=1
fi

# (d2) FUNCTIONAL — the returning client (heavy store on disk) must still REJOIN
# the swarm: its net layer goes live (processes a peer's inv) despite the heavy
# genesis→now replay. Keep a beacon peer alive to supply the inv, then run the
# populate+reload peer. (Generous budget — the replay can hog the main thread; the
# point is "a populated store still converges", which pre-fix it never did.)
peer beacon regbeacon "HOLD_MS=180000" > /tmp/regbeacon.log 2>&1 &
BEACON_PID=$!
for s in $(seq 1 30); do grep -q "beacon up" /tmp/regbeacon.log && break; sleep 1; done
if peer regression reg "SEED_GENS=${SEED_GENS:-800}" "REG_LIVE_MS=${REG_LIVE_MS:-120000}" > /tmp/reg.log 2>&1; then
  echo "  REJOIN: PASS"; grep -E "populated store|REGRESSION ok" /tmp/reg.log | sed 's/^/    /'
else
  echo "  REJOIN: FAIL"; sed 's/^/    /' /tmp/reg.log; FAIL=1
fi
stop_peer regbeacon "$BEACON_PID"

echo
if [ "$FAIL" -eq 0 ]; then echo "==> ALL CONNECTIVITY CHECKS PASSED"; else echo "==> CONNECTIVITY CHECKS FAILED"; fi
exit "$FAIL"
