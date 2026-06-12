#!/usr/bin/env bash
# Build the WASM gallery: compile flame-wasm to wasm32, generate JS bindings,
# and (if available) shrink with wasm-opt. Output lands in web/pkg/.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "==> cargo build (wasm32)"
cargo build --release -p flame-wasm --target wasm32-unknown-unknown

WASM=target/wasm32-unknown-unknown/release/flame_wasm.wasm
OUT=web/pkg

echo "==> wasm-bindgen"
wasm-bindgen --target web --no-typescript --out-dir "$OUT" "$WASM"

if command -v wasm-opt >/dev/null 2>&1; then
  echo "==> wasm-opt -O3"
  wasm-opt -O3 "$OUT/flame_wasm_bg.wasm" -o "$OUT/flame_wasm_bg.wasm"
fi

echo "==> done. Serve with:  python3 -m http.server -d web 8000"
