# wasm-sheep architecture

**Electric Sheep, reborn as a static site + browser swarm.** A GitHub
Pages-deployable site serves a WASM fractal-flame renderer. Each sheep is a
**community-rendered animated artifact**: visitors contribute deterministic
render work into the sheep's shared accumulation, which improves the loop *for
everyone*, and each verified contribution earns a vote that drives selection.
There is no application server — all state lives in the swarm — and because the
renderer is deterministic, every pixel on screen is provably a render of a
public genome, never attacker-supplied data.

> **v2 redesign (this document).** Earlier the unit of work was a *personal
> loop-proof per voter* (each client rendered the whole sheep; that proof was
> their vote) with low/med/high/ultra quality tiers. That is replaced by the
> model below: one sheep, one ever-improving high-resolution render, built
> collectively from verifiable sample-batches. See **Implementation plan** for
> the staged migration.

```
┌─────────────────────┐     static assets only (wasm, js, seed genomes)
│   GitHub Pages      │────────────────────────────────┐
└─────────────────────┘                                ▼
┌─────────────────────┐  WebSocket   ┌──────────────────────────────────┐
│   Relay node        │◄────────────►│  Browser peer                    │
│  (circuit relay v2) │   signaling  │  ┌────────────┐ ┌─────────────┐  │
│   holds no          │   WebRTC     │  │ flame-core │ │ js-libp2p   │  │
│   authority         │  ┌──────────►│  │  (wasm)    │ │ (gossipsub) │  │
└─────────────────────┘  │ p2p data  │  └────────────┘ └─────────────┘  │
                         │           │  ┌────────────────────────────┐  │
   other browser peers ◄─┘           │  │ IndexedDB: sheep, batches, │  │
                                     │  │ render cache, fraud, id     │  │
                                     └──┴────────────────────────────┴──┘
```

## Design principles

1. **Determinism is the root of trust.** `flame-core` renders any unit of work
   `(genome, seed)` byte-identically on every target (all transcendentals via
   the `libm` crate; a guard test forbids std float math outside `fmath.rs`).
   Every claim — "I rendered this batch", "this is fraudulent", "these are the
   children" — is a pure function of public data, checkable without trusting
   anyone.
2. **The sheep is the shared object.** What the swarm holds and grows is the
   sheep itself: its genome plus the accumulated render of its animation loop.
   There is no separate "ledger" to reason about — a sheep simply carries the
   set of sample-batches it contains (its coverage), the way a torrent carries
   its piece list. Contributing = adding batches; sharing = batches flowing to
   other holders; merging two copies = union of their batches.
3. **Nothing displays unless it is validated to be a render of the genome.**
   This is a hard gate, not best-effort (see **Verification**). Shared pixels
   are only ever an optimization to save CPU; the source of truth is always
   "render the genome", and any client can fall back to rendering locally with
   zero trust.
4. **Work is quality is votes.** A contributed batch simultaneously (a) makes
   the sheep look better for everyone and (b) earns its contributor a vote. So
   a sheep's render quality, its popularity, and its selection weight are the
   same number — the cumulative honest render work invested in it. Honest users
   never do makework; only attackers experience the render cost as a cost.

## Components

| Component | Tech | Role |
|---|---|---|
| `flame-core` | Rust | Deterministic renderer: integer-histogram chaos game, batch rendering, tone map, interpolation, crossover/mutation |
| `flame-wasm` | wasm-bindgen | Browser bindings: batch render/verify, histogram merge + tonemap, hashing, breeding, canonicalization |
| `flame-cli` | Rust | Dev tool; native rendering + auditing |
| `web/` | JS, no framework | GUI + swarm logic: gossip, IndexedDB store, accumulation/verify/selection engines |
| Relay | one tiny always-on host | libp2p circuit relay v2 + bootstrap + anchor peer |

## Identity

A peer identity is an Ed25519 keypair (the libp2p PeerId), generated on first
visit, kept in IndexedDB. Identities are free; the protocol never assumes
otherwise. Votes cost honest render work, not identity.

## Data model

All IDs are SHA-256 over canonical bytes. All signed messages are immutable
once published. Time divides into **generations** (5 minutes, wall-clock
aligned, public schedule, no coordination).

### Sheep

```
sheep_id = H(canonical_genome_json)
{
  genome:  <flame-core genome JSON, canonicalized (sorted keys, fixed floats)>,
  parents: [sheep_id, sheep_id] | [sheep_id] | null,  // 2=bred, 1=mutant, null=immigrant/seed/release
  gen:     u64,                       // generation it entered the flock
  origin:  "seed" | "release" | "pair" | "mutant" | "immigrant",
  author:  pubkey | null,            // set for user releases; null for derived
  sig:     ...                       // present for releases; absent for derived (recomputable)
}
```

Sheep are content-addressed: the same genome submitted twice — or bred
independently by two partitioned clients — is one sheep. A genome is an IFS:
a few dozen affine coefficients, variation weights, and a palette. **The medium
cannot represent an arbitrary image** (no photo/text/"scare image" fits in it);
the worst a hostile genome yields is an ugly *fractal*. Genomes are
bounds-checked at ingest (finite params, weights in range, xaos rows well
formed); out-of-bounds genomes are dropped.

### Batch (the unit of work, contribution, and vote)

A flame is rendered by the chaos game, whose trajectories hop all over the
image plane — so you cannot split the work *spatially*. You split the **stream
of input points**. A **batch** is a deterministic slice of that stream for one
animation frame:

```
batch_seed(sheep_id, frame, idx) = first 8 bytes of H(sheep_id ‖ "b" ‖ le32(frame) ‖ le32(idx))
```

Rendering batch `(frame f, idx i)` plots a fixed number of samples (`BATCH_SPP`)
of the genome animated to phase `f / N_FRAMES`, into an integer histogram.
Because the seed and the renderer are deterministic, **every peer who renders
`(f, i)` produces a byte-identical histogram** with a byte-identical hash. A
frame's full render is the sum of its rendered batches; the loop is `N_FRAMES`
such frames. Global `(f, i)` indices mean a sheep's coverage is a compact set
of integer ranges, and two peers rendering the same `(f, i)` produce identical
data (a harmless, idempotent duplicate).

A **contribution record** (signed, gossiped — small):

```
batch_key = sheep_id ‖ ":" ‖ f ‖ ":" ‖ i
{
  sheep_id:    ...,
  frame:       u32,
  idx:         u32,
  hash:        H(integer batch histogram),   // commits to the render output
  spp:         u32,                           // samples in this batch (= BATCH_SPP)
  contributor: pubkey,
  gen:         u64,                           // generation the work counts toward
  sig:         sign(contributor, "batch|sheep_id|f|i|hash|spp|gen")
}
```

It is verifiable by re-rendering `(f, i)` and checking `hash` (the audit
primitive). Each accepted, non-duplicate, verified batch is **one vote for that
sheep in its generation**.

### Accumulated render (the sheep's pixels — the shared artifact)

Per sheep, per frame, peers hold the **summed integer histogram** of that
frame's batches, plus the **coverage set** of which `(f, i)` are included.
Integer/fixed-point accumulation (see **Renderer**) makes the sum
**order-independent and bit-identical**, so any two peers with the same
coverage have byte-identical pixels and merge trivially (union the coverage,
add the histograms). Two fidelities of the same sheep travel the network:

- **Heavy:** the integer frame histograms (what a *contributor* needs to add
  samples and what verification operates on).
- **Light:** the tonemapped frames / short video (what a *viewer* needs to
  watch). Derived from the heavy form; ~a few MB for a whole loop, like the
  original Electric Sheep clips.

### Fraud proof

```
{
  batch_key: sheep_id:f:i,    // the offending contribution
  expected:  H(...),          // what (f,i) actually hashes to
  reporter:  pubkey,
  sig:       ...
}
```

Objectively verifiable: any peer re-renders `(f, i)` (cheap, one batch) and
confirms the contributor signed a wrong hash. On a confirmed fraud proof a
client discards **all** contributions ever signed by that key and excludes its
votes everywhere. Keys are free, but a fresh key restarts at zero votes and
pays full render cost per batch — fraud never beats honesty on cost.

## Renderer (flame-core)

**Integer histograms (the keystone).** A histogram cell accumulates
`[r, g, b, count]` as integers: each plotted sample adds its palette color
quantized to fixed-point (`u16` per channel) and `1` to the count, into `u64`
accumulators. Consequences:

- **Order-independent merge.** Integer addition is exactly associative and
  commutative, so summing the same batches in any order — or merging two peers'
  partial sums — yields identical bytes. This is what makes a shared,
  convergent, content-addressable sheep possible (floats would diverge by ULPs
  and break content-addressing).
- **Exact verification.** Total `count` over all cells equals the sum of
  contributed `spp` exactly; subtracting a known batch can never make a cell go
  negative. Both checks below become provable, not statistical.

The tone map reads the integer histogram (dividing fixed-point color back to
`[0,1]`) and is otherwise the current pipeline, which is **built for partial
data** because a flame is a Monte Carlo estimate — any subset of batches is an
unbiased, noisier estimate of the *same* image:

- **Sample-invariant exposure:** density is normalized by the mean nonzero
  density, so the tone curve is identical at 10 batches or 10,000 — a partial
  render looks like the finished one with more grain, never a different
  brightness.
- **Absolute-count density estimation:** the DE blur radius keys to a cell's
  absolute sample count, so sparse partial renders read as smooth glow and
  resolve into solid structure as batches accumulate (a ratio-based radius
  would blur the dim majority into permanent "fog").

These two properties (already in `render.rs`) are precisely what make a sheep
coherent at every coverage level and monotonically improving.

## Verification — nothing renders to screen unverified

Two independent walls; either alone is strong.

**Wall 1 — the medium can't hold an arbitrary image.** A sheep is an IFS
genome; the renderer can only produce a fractal flame. There is no path for an
uploaded photo/text to become a sheep. Bounds-checking genomes at ingest closes
the only crafted-input surface.

**Wall 2 — pixels are never trusted, only verified.** Shared render data is an
optimization; the source of truth is the genome. Before any accumulated render
is displayed or merged:

1. **Count conservation:** the histogram's total `count` must equal the sum of
   the `spp` of the batches it claims to contain. Extra (injected) density fails
   this exactly (integer arithmetic).
2. **Spot re-render:** re-render a random sample of the claimed batches and
   compare hashes; integer-subtract them from the sum and confirm no cell goes
   negative (the batch is genuinely present). Any mismatch ⇒ discard the whole
   copy and publish a fraud proof against the source; the signed contribution
   makes the ban attributable.
3. **Dominance bound:** a *visible* forgery must dominate the histogram, i.e.
   require many fake batches, so spot re-render catches it with overwhelming
   probability. A handful of fakes cannot form a visible image; a visible image
   cannot hide.
4. **Zero-trust fallback:** any client may ignore shared pixels entirely and
   render the sheep from its genome — guaranteed correct, no network trust. A
   "strict" display mode shows only locally-rendered or fully-verified data.

This is strictly stronger than the original Electric Sheep, where clients
trusted the server's rendered video outright.

## Sharing the sheep (anti-entropy, torrent-style)

The contribution records (small, signed) gossip normally and define each
sheep's coverage and tally. The heavy pixels move on demand:

- A peer announces, per sheep, a compact **coverage digest** (per-frame batch
  ranges, hashed). Peers with gaps request the missing frame histograms (or the
  light video); the server streams them; the receiver runs the **Verification**
  gate before accepting, then merges (union coverage, add histograms).
- Because merges are integer-exact, copies converge regardless of who rendered
  what or in what order. Popular sheep attract more contributors → more batches
  → smoother, cleaner, more-covered loops; ignored sheep stay rough.
- Contribution coordination is implicit: a contributor renders the
  least-covered frames / next-free `(f, i)` it sees, so work spreads across
  frames without a coordinator. Collisions render identical data and are
  deduped by `(f, i)`.

## Votes, selection, generations

- **Tally** of a sheep in generation `g` = count of distinct verified batches
  contributed to it whose record carries `gen = g`, from non-banned keys.
  (Optionally weighted; v1 = one batch, one vote.) This is also its render
  quality, so "best-looking" and "most-voted" coincide by construction.
- **Selection** is unchanged in shape (`gens.js`): per generation, **niched**
  top-`K` (K=6) by tally survive (fitness-sharing over genome distance so one
  aesthetic can't monopolize), filling empty slots from unvoted living, newest
  first. Survivors breed by cyclic pairing; each active generation also derives
  2 high-rate mutant clones of the top survivors and 1 deterministic immigrant.
  All children/mutants/immigrants are derived from public data — every peer
  computes identical genomes, no consensus.
- **Releases:** a user can submit a bred sheep directly (signed, `origin:
  "release"`), capped per (author, gen). It enters the flock immediately and
  earns votes by being rendered like any other.
- **Down-votes (open):** v1 ships the unified model (contributing to a sheep is
  a vote *for* it; you starve sheep you dislike of work and they get out-bred).
  An optional spendable token for explicit culling is a later addition; the
  generation engine already supports signed-direction tallies.

The clock-derived generation schedule and content-addressed, recomputable
children mean a late-arriving batch retroactively recomputes a tally and every
peer self-heals to the same flock.

## Network

Transport interface (`send(msg)` / `onMessage(fn)` in `web/js/net.js`), two
implementations:

- **BroadcastChannel (dev):** same-origin tabs form a real swarm; `?peer=N`
  namespaces identity + store per tab.
- **js-libp2p (production):** WebSocket (browser ↔ relay) + WebRTC (browser ↔
  browser via circuit relay v2). Browsers can't accept inbound; the relay only
  introduces peers, then data flows direct. `?relay=<multiaddr>` /
  `localStorage.relays` override `config.js` RELAYS for local testing.
- **Gossip:** small signed records — `sheep`, `batch` (contribution), `fraud`.
  Heavy render data is **never** broadcast; it is fetched point-to-point on
  demand (`want-render` / `render-data`) and verified before use.
- **Anti-entropy:** jittered `inv` beacons carry one digest per (kind,
  generation) bucket and per-sheep coverage digests; only mismatched buckets
  exchange keys, then records — O(generations + sheep) beacon size, not
  O(records). Every browser persists what it sees and serves it onward.
- **Ingest gate (before storing or relaying):** valid signature; genome parses
  and is in bounds; batch references a known sheep and a live generation;
  dedup; per-peer rate limits; render data passes **Verification**. Invalid ⇒
  dropped, not propagated.

## Cold start and the anchor peer

1. **Baked state:** the static site ships seed genomes and a deploy-time
   snapshot of sheep + their current coverage/light renders, so an empty swarm
   still shows a full, already-decent gallery.
2. **Anchor peer:** the relay host also runs an ordinary peer with disk
   persistence — subscribes, stores, serves sync, contributes batches when
   idle. **No authority**; just a peer that never sleeps, on hardware we own.
3. **Long-lived tabs:** the site is a screensaver; left-open tabs are organic
   anchors and idle contributors.

## GUI

- **Flock view:** gallery of current-generation sheep at thumbnail scale —
  downscaled tonemaps of the *same* shared histograms, sharpening live as
  batches arrive. A "contribute" affordance pledges idle CPU to a sheep.
- **Sheep view:** the full-resolution animated loop, sharpening as the swarm
  (and you) contribute; lineage; a samples/pixel quality readout. **One**
  excellent render — no quality tiers.
- **Idle contribution:** a background worker renders batches for under-covered
  frames of sheep the user is watching / has pledged to, publishes records,
  earns votes.
- **Breeding lab:** pick two living sheep → the canonical child they'd have this
  generation (the preview *is* the deterministic child), plus what-ifs.
- **Network panel:** peers, sync status, coverage of viewed sheep, audit/verify
  activity ("verified 14 batches, 0 frauds").

All rendering runs in a Web Worker pool; the main thread never blocks.

## WASM API (flame-wasm) — contract

Integer-histogram era. `Hist` is a transferable typed array of `u64` cells
(`[r16.16, g, b, count]` fixed-point), length `w·ss·h·ss·4`.

- `render_batch(genome_json, sheep_id, frame, idx, w, h, ss, spp) -> { hash, hist }`
  — deterministic batch render; `hist` integer, `hash = H(hist bytes)`.
- `batch_hash(genome_json, sheep_id, frame, idx, w, h, ss, spp) -> hash`
  — audit primitive (hash only, no pixel return).
- `merge_into(acc: Hist, add: Hist)` — integer add (may live in JS over the
  typed arrays; exact either way).
- `tonemap_hist(hist: Hist, genome_json, w, h, ss) -> rgba` — integer-histogram
  tone map (sample-invariant exposure + absolute-count DE), for any coverage.
- `total_count(hist) -> u64` and `subtract_check(acc, batch) -> bool` —
  verification helpers (count conservation, non-negative subtraction).
- `sheep_id(genome_json)`, `canonicalize(genome_json)`,
  `breed(a, b, challenge)`, `mutate_genome(genome, challenge, rate)`,
  `random_genome_json(seed, transforms)`, `animated(genome, phase)` — unchanged.

## Implementation plan (staged; verification gate is first-class throughout)

Contracts above are fixed so waves can be built against disjoint files.

1. **Renderer foundation (`crates/`).** Integer/fixed-point `Accum`; deterministic
   `render_batch` / `batch_hash` (animation phase + batch seed); integer
   `tonemap_hist`; verification helpers; regenerate goldens (these hashes *do*
   change — this is the protocol break); fmath guard intact; `./web/build.sh`
   produces a working wasm. **Gate:** native goldens pass, browser-vs-native
   determinism for `render_batch` holds.
2. **Protocol (`web/js/net.js`, `store.js`).** `batch` contribution records;
   per-sheep coverage; `want-render`/`render-data` with the Verification gate;
   coverage-digest anti-entropy; fraud over batches; store schema for sheep +
   batches + accumulated render cache. Wire/store version bump.
3. **Engine + UI (`web/js/app.js`, `gens.js`, `audit.js`, `sheep.html`).** Idle
   batch contribution loop; accumulate + verify + tonemap display (flock
   thumbnails and the one-tier sheep view); selection tally from batch counts;
   auditor re-rendering batches. Remove quality tiers and the loop-proof vote.
4. **Tests (`e2e/test.js`) + docs.** Determinism of `render_batch`; two-peer
   contribute → shared sheep improves on both → tally/selection; **forged
   render-data rejected by the Verification gate**; fraud → ban; generation
   engine with batch tallies.

Migration note: wave 1 changes the bitstream, so all prior proofs/goldens are
invalidated by design — there is no in-place compatibility, and the wire/store
versions bump (a clean break, acceptable pre-launch).

## Known limits and open questions

- **Sybil floor.** One vote ≈ one batch ≈ seconds of honest CPU; a native farm
  renders faster than browsers. Bounds, not eliminates, manipulation —
  acceptable for art. Reputation weighting (older clean-audit keys count more)
  can raise the floor later.
- **Heavy-render availability.** A sheep nobody seeds at full fidelity must be
  re-rendered from its genome by a viewer (CPU, not bandwidth). The light video
  cache + the anchor peer mitigate; popular sheep are always well-seeded.
- **Coverage coordination at scale.** Implicit "render the least-covered frame"
  can waste work under high churn (collisions render identical data — wasted
  CPU, never wrong data). Partitioning `(f, i)` space by peer-id hash is the
  escape hatch if measured waste is high.
- **Clock skew / reorg UX, relay SPOF, gossip flooding, eclipse** — as before:
  generation acceptance window, multiple relays, validate-before-relay, and the
  fact that audits/derivations are objective so an attacker can hide state but
  never forge it.
- **Tuning constants.** `N_FRAMES`, `BATCH_SPP`, render resolution, K, audit
  fraction, generation length, fixed-point bits — all TBD by experiment;
  version them in a protocol config.
