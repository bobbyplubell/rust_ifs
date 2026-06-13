#!/usr/bin/env bash
# Swarm stress test in Docker (no host Node). Defaults are smoke-test sized;
# crank PEERS/MINUTES on a big box. Examples:
#   ./stress/run.sh                          # 12 peers, 3 min smoke
#   PEERS=300 MINUTES=30 ./stress/run.sh     # real stress (EPYC-class)
#   PEERS=600 MINUTES=60 RENDER_SLOTS=24 VOTE_RATE=20 ./stress/run.sh
set -euo pipefail
cd "$(dirname "$0")/.."

PEERS="${PEERS:-12}"
MINUTES="${MINUTES:-3}"

docker run --rm \
  --shm-size="${SHM:-4g}" \
  ${MEM_LIMIT:+--memory="$MEM_LIMIT"} \
  -e PEERS="$PEERS" -e MINUTES="$MINUTES" \
  -e WORKERS="${WORKERS:-1}" -e RENDER_SLOTS="${RENDER_SLOTS:-4}" \
  -e VOTE_RATE="${VOTE_RATE:-4}" -e BREED_RATE="${BREED_RATE:-0.5}" \
  -e SAMPLE="${SAMPLE:-20}" -e OUT=/repo/stress/out.jsonl \
  -v "$PWD":/repo:z -w /repo/stress \
  mcr.microsoft.com/playwright:v1.53.0-noble \
  bash -c 'npm install --no-audit --no-fund >/dev/null 2>&1; node swarm.js'
