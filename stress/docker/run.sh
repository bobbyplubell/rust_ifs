#!/usr/bin/env bash
# Container-per-peer swarm stress over the libp2p relay.
#   PEERS=20 MINUTES=5 ./stress/docker/run.sh
set -euo pipefail
cd "$(dirname "$0")"
PEERS="${PEERS:-3}"; export MINUTES="${MINUTES:-3}"
# Ensure stress/'s node_modules has playwright (peers mount it).
[ -d ../node_modules/playwright ] || (cd .. && docker run --rm -v "$PWD":/s:z -w /s node:22 npm install --no-audit --no-fund >/dev/null)

echo "==> starting relay"
docker compose up -d --build relay swarm
# Scrape the relay's (persisted) peer id for the multiaddr.
for i in $(seq 1 30); do
  PID=$(docker compose logs relay 2>/dev/null | grep -oE '12D3KooW[0-9A-Za-z]+' | head -1) && [ -n "$PID" ] && break
  sleep 2
done
[ -n "${PID:-}" ] || { echo "no relay peer id"; docker compose down; exit 1; }
export RELAY_MADDR="/dns4/swarm/tcp/443/tls/ws/p2p/$PID"
echo "==> relay $RELAY_MADDR"
echo "==> launching $PEERS peer containers for ${MINUTES}min"
docker compose up --scale peer="$PEERS" --no-recreate peer 2>&1 | grep -E "DUMP|up; relay|done|ERR|console.error" || true

echo "==> convergence check"
docker compose logs peer 2>/dev/null | grep DUMP | sed 's/.*DUMP //' \
  | docker run --rm -i node:22 node -e '
let lines=require("fs").readFileSync(0,"utf8").trim().split("\n").filter(Boolean).map(JSON.parse);
const last={}; for(const d of lines) last[d.pub]=d; const ds=Object.values(last);
// Primary signal: do all peers hold the same replicated batch SET? (tally view
// is current-gen only and wall-clock sensitive, so report it just as info.)
const sets=new Set(ds.map(d=>d.batchSetHash)), b=ds.map(d=>d.batches);
const views=new Set(ds.map(d=>d.tallyFingerprint));
const spread=Math.max(...b)-Math.min(...b);
console.log(`peers=${ds.length} batches[min/max]=${Math.min(...b)}/${Math.max(...b)} spread=${spread} distinctBatchSets=${sets.size} (tallyViews=${views.size}) -> ${sets.size===1?"CONVERGED":"DIVERGED"}`);'
docker compose down
