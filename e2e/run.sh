#!/usr/bin/env bash
# Run the end-to-end stack test in Docker (real Chromium via Playwright).
# No Node on the host. Usage: ./e2e/run.sh
set -euo pipefail
cd "$(dirname "$0")/.."

docker run --rm -v "$PWD":/repo:z -w /repo/e2e \
  mcr.microsoft.com/playwright:v1.49.1-noble \
  bash -c 'npm install --no-audit --no-fund >/dev/null 2>&1; node test.js'
