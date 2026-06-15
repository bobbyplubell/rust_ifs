# Coordinator API contract (v2)

The single integration surface between the static client and the coordinator.
Both sides build against THIS. JSON over HTTPS; no WebSocket (SSE only if needed).
All mutating requests are **signed** with the caller's Ed25519 key (see Auth).

Base path: `/api`. Errors: `{ "error": "msg" }` + non-2xx.

**Plain REST only — no WebSocket, no SSE.** Live state (flock, vote counts,
improving video, gen countdown) is delivered by the client **polling**
`GET /api/flock` (and `GET /api/me`) every few seconds — this app isn't
real-time-critical, polling is plenty, and it sidesteps the entire
upgrade-handshake failure class that broke v1. The contribute loop is already
request/response (`/assign` → `/submit`), so nothing needs push.

## Identity & auth
- Client identity = an Ed25519 keypair (reuse `web/js/identity.js`; `pubHex` =
  hex public key). Coordinator verifies with ed25519 (e.g. `ed25519-dalek`).
- Every mutating request carries `pub` (hex pubkey), `nonce` (client counter or
  timestamp ms), and `sig` (hex Ed25519 signature over the canonical message).
- **Canonical message** = the request's JSON body with `sig` omitted, keys
  sorted, compact (no whitespace), UTF-8. Both sides implement this identically.
- Reject stale/replayed nonces (keep last-seen per pubkey).

## Render unit contract (determinism is the trust anchor)
A work unit names exactly the args of `flame-core`'s `render_batch`. Client
renders with `flame-wasm`, coordinator re-renders/verifies with native
`flame-core` → identical `hash` for identical args.

WorkUnit:
```
{ sheepId, genomeJson, frame, idx, w, h, ss, spp, nFrames }
```
Result (one per WorkUnit):
```
{ sheepId, frame, idx, hash, count, hist }   // hist = base64(zstd/deflate of the
                                              //   u64 histogram contribution)
```
`hash`/`count` come straight from `render_batch`. `hist` lets the coordinator
merge without re-rendering (compute offload); coordinator MAY re-render to verify
the hash matches (strategy = its choice; start by verifying ~everything).

## Endpoints

### GET /api/flock
Current living sheep.
```
{ gen, gen_closes_in_ms,
  sheep: [ { id, name, parents:[idA,idB]|null, gen, tiles, backings, video } ] }
```
`video` = URL/path to the merged loop (served/cached, ~1–4 MB). `tiles` = total
accepted tiles. `backings` = vote credits backing it this gen.

### GET /api/sheep/:id
Full detail: the `/api/flock` entry + `{ genome, frames_coverage:[n0,n1,...],
samples, alive, hof }`.

### GET /api/video/:id   (cacheable; CDN-friendly)
The merged loop video for a sheep (302 to a cached file is fine).

### POST /api/assign        (signed)
Body: `{ pub, nonce, sig, sheepId? }` (sheepId optional = let server choose).
Returns a bundle:
```
{ units: [ WorkUnit, ... ],          // distinct idxs — never collide
  audits: [ WorkUnit, ... ] }         // (optional/empty in v2.0) re-render-to-verify tasks
```

### POST /api/submit        (signed)
Body: `{ pub, nonce, sig, results: [ Result, ... ], audit_reports?: [ {sheepId,frame,idx,hash} ] }`.
Coordinator verifies sigs, (re-)verifies, merges accepted pixels, credits the
contributor (128 accepted tiles = 1 credit), bans on fraud.
Returns: `{ accepted, rejected, credits, reputation }`.

### POST /api/vote          (signed)
Body: `{ pub, nonce, sig, sheepId }`. Spends 1 credit to back a sheep this gen.
Returns: `{ ok, credits, backings }`.

### POST /api/breed         (signed)   — propose-a-pairing
Body: `{ pub, nonce, sig, parentA, parentB }`. Spends credits; coordinator does
the crossover, renders + adds the child to the flock.
Returns: `{ childId }`.

### GET /api/me?pub=HEX
`{ credits, reputation, tiles, backings_used }` for that key.

### GET /api/hall
Enshrined (Hall of Fame) sheep, same entry shape as `/api/flock`.

## Coordinator-internal (not client-facing), for reference
- GA tick on the generation boundary: tally votes → select survivors → breed →
  inject fresh-blood immigrants. Stores genomes; clients only ever fetch them.
- Merge state = accumulated u64 histogram per (sheep, frame); canonical small
  state (genomes/votes/credits/reputation/tile-tallies) in SQLite; histograms +
  videos are regenerable on-disk caches.
- Encode video on a quality-delta threshold (tonemap via flame-core → ffmpeg),
  cache it, serve via /api/video.
- Moderation: operator can remove any sheep (controls the flock).

## Generation timing
`GEN_MS` and `nFrames`/spec live server-side now (no client replay). The client
reads `gen` + `gen_closes_in_ms` from `/api/flock`.
