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
merge without re-rendering.

**Trust model = ingest-trust + peer-audit (the coordinator does NOT re-render on
the happy path).** On `/submit` the coordinator:
1. verifies the signature;
2. decodes `hist` and **content-hashes the uploaded pixels** — that hash becomes
   the tile's canonical content hash (no render);
3. as a cheap self-consistency gate, requires the submitter's claimed `hash` to
   equal that content hash (rejects garbled uploads — still no render);
4. merges the uploaded pixels into the accumulation and records the tile as
   **`unaudited`** in the ledger, tagged with the submitter's pubkey + content
   hash, and credits the contributor **provisionally**.

The tile is later **validated by a PEER** (see `assign.audits` /
`submit.audit_reports`), not by the coordinator re-rendering it. The coordinator
re-renders a tile only on a corroborated **audit dispute** (rare) or to **grade a
honeypot** (free — the answer was planted) — never to verify an honest ingest.

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
{ units:  [ WorkUnit, ... ],   // your render work — distinct idxs, never collide
  audits: [ WorkUnit, ... ] }  // peer-audit tasks: re-render & report the hash
```
`audits` is a **reputation-weighted sample of OTHER contributors' `unaudited`
tiles** (never your own; new/low-reputation submitters are audited heavily,
trusted ones lightly), plus the occasional **honeypot** (a tile the coordinator
already knows the answer to — indistinguishable from a real audit on the wire).
Each audit task is a WorkUnit (genome/frame/idx/spec) **without** the submitter's
claimed hash. The client renders each through the same `render-batch` path and
reports the observed hash in the next `/submit` (`audit_reports`).

### POST /api/submit        (signed)
Body: `{ pub, nonce, sig, results: [ Result, ... ], audit_reports?: [ {sheepId,frame,idx,hash} ] }`.
Two flows in one call:
- **`results`** — your rendered tiles. Each is **trust-ingested**: sig verified,
  pixels content-hashed + merged, tile recorded `unaudited`, credit provisional
  (128 accepted tiles = 1 credit). **No re-render.** A claimed-hash ≠
  uploaded-pixels-hash result is rejected on the cheap self-consistency gate.
- **`audit_reports`** — hashes you observed re-rendering the `audits` you were
  handed. The coordinator grades each:
  - real audit, hash == the tile's stored content hash → tile becomes **`audited`**
    (validated); submitter + auditor reputations bump.
  - real audit, hash mismatch → **DISPUTE** (once corroborated by a trusted or a
    second auditor): the coordinator re-renders that ONE tile to find ground
    truth → the party whose hash ≠ truth is banned; a fraudulent submitter's
    merged contribution is subtracted from the accumulation.
  - honeypot, wrong answer → the auditor didn't actually re-render → penalized /
    banned.

Returns: `{ accepted, rejected, credits, reputation, audits_validated, disputes }`.

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
