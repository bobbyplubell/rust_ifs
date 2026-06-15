# proof-of-sheep coordinator (v2)

The single stateful service for the v2 architecture: an `axum` + `tokio` REST
API that runs the genetic algorithm, hands out deterministic render work-units,
verifies + merges contributions natively via `flame-core` (the SAME code the
client runs as WASM), and serves the merged loop videos. See `../ARCHITECTURE.md`
and `../API.md` for the contract.

**Plain REST only** — no WebSocket, no SSE. The client polls `/api/flock` and
`/api/me`.

## Run locally

```sh
# from the repo root
cargo run -p coordinator
```

Env knobs (all optional):

| var          | default            | meaning                                   |
|--------------|--------------------|-------------------------------------------|
| `BIND`       | `0.0.0.0:8080`     | listen address                            |
| `DATA_DIR`   | `./data`           | SQLite db + regenerable hist/video caches |
| `GENOMES_DIR`| `../web/genomes`   | seed flock source on first boot           |
| `GEN_MS`     | `86400000` (24h)   | generation length                         |
| `RUST_LOG`   | `coordinator=info` | log filter                                |

On first boot it seeds the flock from `web/genomes/*.json` (skipping
`manifest.json`), tops up to 8 sheep with random immigrants, and creates
`<DATA_DIR>/coordinator.sqlite` (WAL mode).

## Quick curl smoke

```sh
# 1. the flock (public, unsigned)
curl -s localhost:8080/api/flock | jq

# 2. one sheep's detail
curl -s localhost:8080/api/sheep/<id> | jq

# 3. the merged video (302/200 to a cached webm once tiles have merged)
curl -s -o sheep.webm localhost:8080/api/video/<id>
```

The mutating endpoints (`/assign`, `/submit`, `/vote`, `/breed`) require an
Ed25519 signature (see Auth below), so the full contribute round-trip is easiest
to exercise via the integration test:

```sh
cargo test -p coordinator --test smoke -- --nocapture
```

That test boots the binary, calls `/api/flock` → `/api/assign`, renders the
assigned tiles with `flame-core` exactly as the WASM client would, posts them to
`/api/submit`, asserts they're accepted + credited, and asserts a tampered
submit (wrong hash) is banned with HTTP 403.

## Auth (matches `web/js/identity.js`)

Every mutating request is a JSON object carrying `pub` (hex Ed25519 public key),
`nonce` (monotonic counter or ms timestamp), and `sig` (hex Ed25519 signature).

- **Canonical message** = the request body with `sig` removed, object keys
  sorted (recursively), serialized compact (no whitespace), UTF-8.
- The client signs those exact bytes; the coordinator reconstructs them and
  verifies against `pub`.
- Nonces are last-seen-per-pubkey and must strictly increase (replay rejection).

## Endpoints

| method + path        | signed | status   |
|----------------------|:------:|----------|
| `GET /api/flock`     |   no   | working  |
| `GET /api/sheep/:id` |   no   | working  |
| `GET /api/video/:id` |   no   | working  |
| `POST /api/assign`   |  yes   | working  |
| `POST /api/submit`   |  yes   | working  |
| `POST /api/vote`     |  yes   | working  |
| `POST /api/breed`    |  yes   | working  |
| `GET /api/me`        |   no   | working  |
| `GET /api/hall`      |   no   | working  |
| `GET /health`        |   no   | working  |

## The core loop

1. **`/assign`** picks a sheep (requested, else least-covered alive) and inserts
   up to `BUNDLE_SIZE` fresh `(sheep, frame, idx)` tiles into the `tile` ledger.
   The `(sheep_id, frame, idx)` primary key is the collision guard — a tile is
   handed to exactly one pubkey. Returns `WorkUnit`s naming the exact args of
   `flame-core::render_batch`.
2. The client renders each unit with `flame-wasm::render_batch` → `(hash, hist)`.
3. **`/submit`** verifies the signature + nonce, then for each result: confirms
   the tile is assigned to this pubkey, **re-renders the unit natively with
   `flame-core::chunked::render_batch` and checks the hash matches** (v2.0 =
   verify everything), decodes the `hist` (base64 + zstd/deflate, size-bounded),
   confirms the decoded histogram hashes to the same value, then **merges it into
   the accumulated per-(sheep,frame) u64 histogram on disk**. 128 accepted tiles
   = 1 credit. A hash mismatch **bans the account and frees its tiles**.
4. Crossing a tile-count step triggers a best-effort **video re-encode** (tonemap
   each frame via `flame-core` → one rawvideo stream → ffmpeg → looping VP9
   webm), cached under `<DATA_DIR>/video/<id>.webm`.

## Deploy (Docker + Caddy)

```sh
# from the repo root
docker compose -f coordinator/deploy/docker-compose.yml up -d --build
```

`coordinator/Dockerfile` is a multi-stage Rust build (includes `ffmpeg` in the
runtime image; `rusqlite` is `bundled` so no system SQLite needed).
`deploy/docker-compose.yml` runs the coordinator behind Caddy (auto-HTTPS); set
`COORDINATOR_DOMAIN` for the Caddyfile. The coordinator data (SQLite + caches)
lives in the `coordinator-data` volume.

## State

- **SQLite** (`<DATA_DIR>/coordinator.sqlite`, WAL) holds the small canonical
  state: `meta` (gen clock), `sheep` (genomes + lineage + lifecycle),
  `account` (credits/reputation/tiles/nonce/ban), `tile` (the assignment +
  acceptance ledger — collision guard), `vote`, `coverage`.
- The **accumulated histograms** (`<DATA_DIR>/hist/<id>/frame_NNNN.bin`) and the
  **videos** (`<DATA_DIR>/video/<id>.webm`) are regenerable on-disk caches.
