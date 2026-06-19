# proof-of-sheep architecture (v3 — P2P swarm)

**Electric Sheep, reborn — peer-to-peer.** There is no coordinator and no
generations. A **swarm of identical native nodes** (`crates/sheep-node`) gossips
a shared, append-only log of signed events; each node independently computes the
same live **flock** of fractal-flame sheep from that log, renders them, and
accumulates the results. **Browsers** are thin HTTPS clients to any node: they
watch the flock (cached video) and optionally contribute renders + votes. There
is exactly **one global swarm / one shared flock** — nodes are interchangeable
gateways into it, never separate worlds.

This supersedes **v2** (a central coordinator running a genetic algorithm; lives
in git history + `coordinator/`) and **v1** (browser-native libp2p/WebRTC, which
never connected reliably; `attic/v1-p2p-client/`). The browser stays simple by
*not* speaking libp2p — it rides a 1:1 REST skin over the wire protocol, and a
node bridges its writes into the swarm.

The guiding principle is unchanged from v2: **every displayed pixel is a
deterministic render of a signed, reproducible genome.** Work is verifiable;
the heavy data is a *recipe*, not a payload; trust comes from anyone being able
to re-run the math and get identical bytes.

---

## 0. The foundation: determinism

`crates/flame-core` is one renderer compiled two ways — **natively** in the node
and to **WASM** in the browser — and it is *byte-for-byte* deterministic across
both. Transcendentals go through `libm` (`fmath.rs`, lint-enforced: no `f32`, no
FMA); the chaos game, palette, and tonemap are fixed-point and reproducible. A
CI golden pins the output. This is what makes everything else possible: a tile
rendered by an anonymous browser produces the *same* histogram bytes — and thus
the same content hash — as the node re-rendering it to verify. Determinism is
the trust boundary.

---

## 1. Shape — one binary, browser as a thin client

```
        ┌──────────────────── GitHub Pages (static client) ────────────────────┐
        │  watch flock (cached webm) · render assigned tiles (WASM) · vote/mint │
        └───────────────▲──────────────────────────────────────┬───────────────┘
                         │ GET /api/flock,/api/sheep,/api/video  │ POST /api/msg (signed)
                         │ GET /api/assign  (HTTPS, any node)    │ GET /api/assign
        ┌────────────────┴──────────── sheep-node (server mode) ┴───────────────┐
        │  Caddy auto-HTTPS  →  HTTP read/write face  →  run loop                │
        └────────────────▲───────────────────────────────────────┬──────────────┘
                          │            libp2p swarm                │
        ┌─────────────────┴─────────┐                  ┌───────────┴───────────────┐
        │  sheep-node (peer)        │ ◀── gossipsub ──▶ │  sheep-node (peer)         │
        │  engine · accumulator     │   + flock-sync    │  engine · accumulator      │
        └───────────────────────────┘   req/resp        └────────────────────────────┘
```

- **`sheep-node`** — one Rust binary. Peers connect over libp2p (TCP + noise +
  yamux), gossip signed `Envelope`s on topics, and answer request/response
  protocols (`PIECE`, `ASSIGN`, `FLOCK_SYNC`). A node in **server mode** also
  runs the HTTP face (`--http-addr`) so browsers can reach the swarm.
- **Static client** — GitHub Pages, no trusted logic. Talks to one node over
  plain HTTPS; pulls the merged loop video; renders/votes with a local Ed25519
  key. `config.js` lists the gateway nodes (ordered, for failover) — they are
  the *same* swarm, so any of them serves the same flock.
- **Shared renderer** — `flame-core`, native in the node + WASM in the browser
  (`web/js/{worker,pool}.js`). Same code, same bytes (§0).

### 1.1 Node roles are capability bundles, not types

A node is configured by which capabilities it turns on, so it fits any budget:

- **render** — pulls work, renders tiles, gossips them. The minimum useful peer.
- **accumulate** — ingests tiles into the CRDT accumulator and can serve video.
- **serve** — runs the HTTP face (the browser gateway).
- **audit** — re-renders others' tiles to attest/dispute (§6).

A laptop can render-only; a seed VPS does all four. RAM is bounded by config
(§5), so "lightweight" is a dial, not a different program.

---

## 2. No generations — the central simplification

There is no GA, no epoch, no central breeder. The flock is just the set of sheep
currently *alive*, computed locally from the log.

### 2.1 Birth — only by signed user action, genome derived (not supplied)

A sheep is born by a signed **Mint** or **Breed** envelope. Crucially the user
does **not** supply a genome — a genome is untrusted code that gets *executed*
by every renderer. Instead the protocol **derives** the genome deterministically
from the recorded, signed inputs (mint nonce/time, or the two parent ids for a
breed). So a birth is permissionless and verifiable: every node derives the same
genome from the same signed event, and nobody can inject a hand-crafted or
malicious genome. (The swarm also mints a small **founding flock** at boot to
seed an empty world.)

### 2.2 Survival — loved or dead, age-escalating

Each sheep carries **backing** (credits spent to keep it alive). Backing decays
on a closed-form, **age-escalating** curve
`base + linear·t + quad·t² + exp_scale·(2^(t/half_life)−1)` where
`t = age / time_unit`. Young sheep are cheap to keep; old sheep need
exponentially more love to survive. When backing hits zero the sheep leaves the
live flock. Exceptional sheep (long-lived, well-backed) are enshrined in a
**Hall of Fame** before they perish. This replaces generational turnover with a
continuous "loved or dead" pressure — no quorum, no scheduler.

### 2.3 Flock membership — locally computed

Each node computes the live flock by replaying births and applying decay at the
current time. No vote, no consensus round: two nodes with the same log compute
the same flock. Late joiners catch up via `FLOCK_SYNC` (a req/resp birth-log
pull) so they converge without trusting any single peer.

---

## 3. The economy — one currency

One fungible **credit**. The only source is **useful work**: a key earns 1
credit per `TILES_PER_CREDIT` (128) of its tiles that the swarm **confirms**
(audits + accumulates). The only sinks are actions that shape the flock:

| action | cost (R384 base; scales with resolution tier) |
|--------|--------|
| vote / back a sheep | 1 |
| mint a new sheep    | 8 |
| breed two sheep     | 20 |

A key's spendable balance is `earned − spent`, both log-derived; an overspend is
rejected at `apply`. Browsers are first-class here: render → earn → mint/breed/
vote, all with a disposable browser key. Minting is moderately expensive (a
credit sink) but not prohibitive; the real scarcity is *survival* (§2.2), which
self-balances flock density — more sheep means more backing demand per sheep.

---

## 4. Work distribution — neutral claims, no central assigner

No node hands out authoritative work. Each renderer (and the advisory
`/api/assign` for browsers) independently picks the **least-covered, uncapped,
unclaimed** block of the flock. Soft **claims** are gossiped so peers diverge
onto distinct blocks; a claim race just yields a duplicate render that
content-addressing dedups (harmless).

- **Breadth-first ordering** — work sweeps one render-idx across *all* 128 frames
  of a sheep before deepening density, so the full animation becomes visible
  quickly (low density everywhere) instead of a few finished frames + blanks.
- **Per-sheep coverage cap (§4.1)** — a sheep can't run more than
  `min_flock_coverage + tolerance` confirmed tiles ahead of the least-covered
  sheep, so no one starves the rest of the flock by over-rendering a favorite.
- `/api/assign` is **advisory and read-only**; it's served from a cache so a
  browser still gets work while the node is mid-render.

---

## 5. Data flow — heavy data is a content-addressed CRDT

A render unit is a tile: a `(sheep, frame, idx, pass)` histogram. Tiles are
**content-addressed by their bytes** and merged by element-wise integer
addition, which is commutative, associative, and idempotent — so the
**accumulator is a CRDT**: any set of nodes that ingest the same confirmed tiles
reach byte-identical per-frame sums with zero coordination. Fraud retraction is
a keyed removal (subtract a slashed tile's contribution).

- **Memory-bounded** — the accumulator keeps only the *merged* per-(sheep,frame)
  sum plus a set of folded content-hashes (idempotency) — never the raw tiles.
  Merged frames live in a disk-backed **LRU bounded by `SHEEP_ACCUM_RAM_MB`**;
  evicted frames spill (zlib) to `<data>/accum` and stream back on demand. So
  per-node RAM is independent of flock size — a 256 MB node and a 4 GB node both
  work; the budget is just a dial.
- **Video** — a node tonemaps the merged frames and ffmpeg-encodes a looping
  webm, served (and CDN-cacheable) under `/api/video/:id`. While a sheep is only
  partially rendered, encode uses **boomerang** playback (rendered frames forward
  then back) instead of emitting blank frames.
- **Tiles move by gossip today** (bounded by a raised gossipsub max message
  size). A planned evolution is announce-over-gossip + pull-over-`PIECE`
  req/resp, so a node only fetches the heavy bytes it actually wants.

---

## 6. Trust — audits as a shared attestation log

Confirmation is decentralized. A tile is **confirmed** once it carries a valid
**attestation** (an auditor re-rendered it and the hashes matched). Attestations
are themselves signed log events, so trust is a shared, replayable record:

- **Reputation-graduated sampling** — a submitter with a track record of
  confirmed work gets audited less often (but never zero); a newcomer gets
  audited heavily. Reputation is log-derived "proof of useful work."
- **Honeypots** — a node plants tiles whose true hash it already knows; an
  attestation that contradicts the truth proves the attestor is lying.
- **Disputes** — a corroborated hash mismatch triggers one dispute re-render;
  the loser is **slashed** (reputation/credits) and the fraudulent tile is
  **retracted** from every accumulator that holds it.

### 6.1 Gateway ingest-audit (browser write-face)

Browser identities are disposable, so a server-mode node defaults to
**verify-before-vouch**: when a browser POSTs a tile, the gateway re-renders it
and only co-signs/accumulates it if the hash matches. This lets anonymous
browsers earn credits without letting them poison the flock. It is the
`--ingest-audit` policy (on by default for public nodes).

---

## 7. Consistency — per-key sequence numbers

The only consistency primitive is a **per-key monotonic sequence number** on
mutating events (mint/breed/vote). Two differently-signed events at the same seq
from one key is provable **equivocation** (slashable). Everything else is a CRDT
or a pure function of the log, so there is no global lock, leader, or round.

---

## 8. Identity

Ed25519 keypairs. A node persists its key (`--key-file`) for a stable peer id
and durable bootstrap address. A browser generates a key in `localStorage`; the
same key is its identity across every gateway (same swarm → same standing).
Signing is canonical (recursive key-sort, signature field excluded, compact
UTF-8) and **byte-matched between the JS and Rust implementations** — the
integration contract for the REST skin.

---

## 9. Protocol — pinned first (`crates/sheep-proto`)

Every message is a signed **`Envelope { v, t, from, ts, body, sig }`**. The wire
contract lives in its own crate so the node, the browser, and any future client
agree on bytes. The browser face is a mechanical **1:1 REST skin** over these
messages, never a second protocol:

- **reads (watch):** `GET /api/flock`, `/api/sheep/:id`, `/api/video/:id`,
  `/api/hall` — cacheable projections of node state.
- **writes (contribute):** `POST /api/msg` — a signed `Envelope` (or array). The
  node verifies the signature, runs the *same* handler the gossip path runs, and
  gossips it onward. `GET /api/assign?pub=` returns advisory work for a key.

The reply to a write echoes the signer's standing (`accepted`, `credits`,
`confirmed_tiles`, `tiles_per_credit`) so a contributor sees progress live.

---

## 10. Deployment

- **One global swarm.** Production runs two seed nodes (`relay`, `relay2`) that
  **cross-bootstrap** each other and converge on one shared flock; either is a
  valid browser gateway. (The node's libp2p transport has no DNS resolver, so
  bootstrap addresses are `/ip4/.../tcp/4001/p2p/<peerid>`.)
- **Image pipeline.** The droplets can't compile Rust, so GitHub Actions builds
  the `sheep-node` image on native amd64 and pushes it to GHCR; the droplets
  `docker compose pull` it. **Caddy** terminates TLS per domain and proxies the
  HTTP face; the libp2p port is published directly.
- **Static client.** GitHub Pages serves `web/` (auto-deployed on push).
- **Budget knobs.** `SHEEP_ACCUM_RAM_MB` (accumulator LRU budget),
  `SHEEP_BOOTSTRAP_FLOCK` (founding sheep), `SHEEP_BOOTSTRAP` (peer multiaddrs),
  and the decay/cost env all tune a node to its host.

---

## 11. Reuse vs. build

- **Reused wholesale:** `flame-core` (the deterministic renderer + tonemap),
  libp2p (transport/gossip/req-resp), Caddy (TLS), ffmpeg (encode), the WASM
  render pool.
- **Built for v3:** `sheep-proto` (wire contract), `sheep-node` (engine +
  transport + CRDT accumulator + HTTP face + video), and the static v3 client.
- **What's distributed vs. anchored:** births, votes, claims, attestations,
  credits, and reputation are all **distributed** (log-derived, convergent). The
  only "anchor" is each browser's gateway choice — a convenience, not authority,
  since every gateway computes the same flock.
