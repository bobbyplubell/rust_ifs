#!/usr/bin/env bash
# Build the browser libp2p bundle (and run the round-trip test) inside Docker —
# no Node on the host. Output: web/js/vendor/libp2p.js
set -euo pipefail
cd "$(dirname "$0")"

docker run --rm -v "$PWD":/app:z -w /app node:22 bash -c '
  set -e
  npm install
  node test/roundtrip.js
  npx esbuild src/browser-entry.js --bundle --format=esm --minify --outfile=dist/libp2p.js
'

mkdir -p ../web/js/vendor
cp dist/libp2p.js ../web/js/vendor/libp2p.js
echo "==> wrote web/js/vendor/libp2p.js ($(du -h ../web/js/vendor/libp2p.js | cut -f1))"
