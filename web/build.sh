#!/usr/bin/env bash
# Build the WASM gallery: compile flame-wasm to wasm32, generate JS bindings,
# and (if available) shrink with wasm-opt. Output lands in web/pkg/.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "==> cargo build (wasm32)"
cargo build --release -p flame-wasm --target wasm32-unknown-unknown

WASM=target/wasm32-unknown-unknown/release/flame_wasm.wasm
OUT=web/pkg

# Locate wasm-bindgen at the EXACT version the crate is locked to (a mismatch
# produces subtly broken bindings). Prefer one on PATH; otherwise fetch the
# prebuilt binary into ./.tools (gitignored) — no global install needed.
WB_VER=$(grep -A1 'name = "wasm-bindgen"' Cargo.lock | grep -m1 version | sed 's/.*"\(.*\)".*/\1/')
WB=wasm-bindgen
if ! command -v wasm-bindgen >/dev/null 2>&1 || [ "$(wasm-bindgen --version | awk '{print $2}')" != "$WB_VER" ]; then
  WB="$PWD/.tools/wasm-bindgen"
  if [ ! -x "$WB" ] || [ "$("$WB" --version | awk '{print $2}')" != "$WB_VER" ]; then
    echo "==> fetching prebuilt wasm-bindgen $WB_VER"
    mkdir -p .tools
    ARCH=$(uname -m)
    curl -fL "https://github.com/rustwasm/wasm-bindgen/releases/download/${WB_VER}/wasm-bindgen-${WB_VER}-${ARCH}-unknown-linux-gnu.tar.gz" \
      | tar xz -C .tools --strip-components=1 --wildcards '*/wasm-bindgen'
  fi
fi

echo "==> wasm-bindgen ($("$WB" --version))"
"$WB" --target web --no-typescript --out-dir "$OUT" "$WASM"

if command -v wasm-opt >/dev/null 2>&1; then
  echo "==> wasm-opt -O3"
  wasm-opt -O3 "$OUT/flame_wasm_bg.wasm" -o "$OUT/flame_wasm_bg.wasm"
fi

echo "==> done. Serve with:  python3 -m http.server -d web 8000"
