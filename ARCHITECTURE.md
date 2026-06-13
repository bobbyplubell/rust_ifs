# wasm-sheep architecture

**Electric Sheep, reborn as a static site + browser swarm.** A GitHub
Pages-deployable site serves a WASM fractal-flame renderer. Every visitor's
browser renders sheep locally, votes on which survive, and gossips state over
js-libp2p. There is no application server: all state lives in the swarm.
Rendering a sheep is what earns a vote, and — because the renderer is
deterministic — that work is cheaply verifiable by any peer. History is a
hash-chained sequence of **generations**, with the most-cumulative-render-work
chain as the canonical one.

```
┌─────────────────────┐     static assets only (wasm, js, seed genomes)
│   GitHub Pages      │────────────────────────────────┐
└─────────────────────┘                                ▼
┌─────────────────────┐  WebSocket   ┌──────────────────────────────────┐
│   Relay node        │◄────────────►│  Browser peer                    │
│  (circuit relay v2, │   signaling  │  ┌────────────┐ ┌─────────────┐  │
│   tiny VM / Pi —    │              │  │ flame-core │ │ js-libp2p   │  │
│   holds no          │   WebRTC     │  │  (wasm)    │ │ (gossipsub) │  │
│   authority)        │  ┌──────────►│  └────────────┘ └─────────────┘  │
└─────────────────────┘  │ p2p data  │  ┌────────────────────────────┐  │
                         │           │  │ IndexedDB (sheep, votes,   │  │
   other browser peers ◄─┘           │  │ blocks, fraud, identity)   │  │
                                     └──┴────────────────────────────┴──┘
```

## Design principles

1. **Determinism is the root of trust.** `flame-core` renders `(genome, seed)`
   byte-identically on every target. Every claim in the protocol ("I rendered
   this", "this vote is fraudulent", "these are this generation's children",
   "this is the canonical history") is a pure function of public data, so any
   peer can check any claim without trusting anyone.
2. **State is a set of signed, immutable facts.** Sheep, votes, and fraud
   proofs are append-only. There is nothing to reconcile: the CRDT is a
   grow-only set. Everything else — tallies, survivors, children, the
   generation chain itself — is *derived* locally and identically by everyone
   who holds the same facts.
3. **No authority anywhere.** The relay brokers WebRTC handshakes and may cache
   state, but its word counts for exactly as much as any other peer's: zero.
   Generation blocks are never signed — they are recomputed, not believed.
4. **The work is the product.** The proof-of-render cost that gates voting is
   the same rendering the user wanted to watch anyway. Honest users never do
   makework; only attackers experience the cost as cost. And because votes are
   proofs of work, "most votes" and "most work" are the same measure — which
   is what makes a heaviest-work chain possible without a token.

## Components

| Component | Tech | Role |
|---|---|---|
| `flame-core` | Rust | Deterministic renderer: genome, chaos game, tone map, interpolation, (to add) crossover/mutation |
| `flame-wasm` | wasm-bindgen | Browser bindings: chunked rendering, hashing, breeding, genome canonicalization |
| `flame-cli` | Rust | Dev tool; native rendering for testing and (optional) relay-side auditing |
| `web/` | JS, no framework | GUI + swarm logic: gossipsub, IndexedDB store, tally/audit/chain engine |
| Relay | one tiny always-on host | libp2p circuit relay v2 + bootstrap + (optional) anchor peer |

## Identity

A peer identity is an Ed25519 keypair (the libp2p PeerId), generated on first
visit and kept in IndexedDB. Identities are free — the protocol never assumes
otherwise. Votes cost compute, not identity.

## Data model

All IDs are SHA-256 over canonical bytes. All signed messages are immutable
once published. Time divides into **generations** (5 minutes, wall-clock
aligned; the schedule is public and needs no coordination). Population
pressure is built in: survivors are a fixed top-K (K=6) by **net** tally
(votes carry a direction; a down-vote costs the same render proof and
net-negative sheep are culled at the close), automatic births are bounded
(cyclic pairs of survivors, plus 2 high-rate mutant clones of the top
survivors and 1 random immigrant per active generation — all derived from
public data, so every peer computes identical genomes), and submissions cost
a render proof and are capped per (author, generation) — flock size cannot
blow up with peer count. A
work-threshold *early close* ("generation ends after V votes") is designed
but deferred: it would make generation numbering chain-relative instead of
clock-derived, which is a consensus step we don't take until needed.

### Sheep

```
sheep_id = H(canonical_genome_json)
{
  genome:   <flame-core genome JSON, canonicalized>,
  parents:  [sheep_id, sheep_id] | null,   // null for gen-0 / user submissions
  gen:      u64,                           // generation it was born in
  author:   pubkey | null,                 // null for bred children (see below)
  sig:      ...                            // absent for bred children
}
```

Sheep are content-addressed, so the same genome submitted twice — or bred
independently by two partitioned clients — merges into one.

### Vote (with embedded render proof — "Proof of Sheep")

```
challenge_seed = H("v3" ‖ sheep_id ‖ voter_pubkey ‖ gen)
{
  sheep_id:     ...,
  gen:          u64,
  voter:        pubkey,
  chunk_hashes: [H(frame_histogram); M],   // M = 64 (tunable)
  sig:          ...
}
```

**Protocol v3 — loop proofs.** The proof's M units are *frames of the sheep's
animation loop*: frame `i` is the genome animated to phase `i/M`, rendered as
T=2 temporal sub-steps (motion blur) seeded from `H(challenge_seed ‖ i)`, each
frame's histogram hashed independently. Proving a vote therefore means
rendering — watching — one full loop of the sheep (~15M samples, seconds of
background CPU: a real cost, deliberately), and the frames are cached so the
proven sheep replays as an animation afterward. The audit asymmetry is
unchanged: re-render one random frame = 1/M of the cost.

**Proof tiers.** A vote declares a `tier` (signed): `std` (the spec above) is
worth 1 vote; `ultra` (~2.7× the samples at 384px) is worth 2. Spending more
CPU is a choice with a reward — weight plus a denser cached replay — not a
tax, so weak devices stay first-class voters. The challenge is
tier-independent (the hashes differ because the spec does, so a std render
cannot pass as ultra); audits and fraud verification re-render under the
vote's declared tier.

Key properties:

- **Self-certifying challenge.** No server issues nonces. The seed is bound to
  the voter's key and the generation, so proofs can't be copied from another
  peer, precomputed earlier, or reused across generations.
- **Offline-auditable.** Any peer can audit any vote *without contacting the
  voter*: re-render one randomly chosen chunk (1/M of the full cost) and
  compare against the signed `chunk_hashes[j]`. A cheater who faked a fraction
  `f` of chunks is caught with probability `f` per audited chunk.
- **One vote per (voter, sheep, gen).** Duplicates are dropped at ingest.

### Fraud proof

```
{
  vote_ref:  H(vote),        // the offending signed vote
  chunk:     j,
  expected:  H(...),         // what chunk j actually hashes to
  reporter:  pubkey,
  sig:       ...
}
```

A fraud proof is *objectively verifiable*: any peer can re-render chunk `j`
(1/M cost) and confirm the voter signed a wrong hash. This makes negative
gossip trustless — you don't have to believe the reporter, you check. A false
accusation is itself checkable and discredits the reporter instead. On a
confirmed fraud proof, a client locally discards **all** votes ever signed by
that key. (Keys are free, but every fresh key restarts at zero votes and full
render cost per vote — fraud never beats honesty on cost.)

### Generation block (derived — never signed, never gossiped as authority)

```
block = {
  gen:        u64,
  prev:       block_hash,                  // chain link
  votes_root: merkle_root(valid votes tallied for this gen, sorted),
  survivors:  [(sheep_id, tally); K],      // top K by audited tally
  children:   [sheep_id; ...],             // bred from survivors (below)
}
block_hash = H(canonical block)
```

At each generation boundary every client computes the block **locally** from
the votes it holds. Two clients that saw the same votes produce byte-identical
blocks, so a block hash is a one-word answer to "do we agree about generation
G?" — anti-entropy sync uses it as its first probe and only drills down on
mismatch.

## The generation chain

### Fork choice: heaviest render-work

Different vote sets → different blocks → forks. Since every valid vote is a
proof of expended rendering work, the fork-choice rule is Nakamoto's, minus
the token: **prefer the chain with the most cumulative audited votes** (ties:
lower block hash). A client that learns of votes it missed recomputes affected
blocks and reorgs if the result is heavier.

Reorgs are cheap and non-tragic: no balances move. A reorged generation means
a different set of canonical children; the orphaned children still exist as
content-addressed sheep — they merely lose canonical lineage. (Anyone who
loves an orphan can resubmit it as a signed gen-0 sheep: "adoption.")

### Finality and pruning

A block `W` generations deep (start: W = 2) is final locally: votes arriving
later are ignored for history. After finality, vote *bodies* can be pruned —
the block's `votes_root` remains as the permanent commitment, and the chain of
blocks + sheep genomes (both small) is the durable record. New clients sync
the chain and all sheep cheaply, and only fetch + audit vote bodies for the
unfinalized window. The full vote history is thus bounded: state size grows
with sheep, not with votes.

### Breeding (no coordinator)

Survivor selection is **niched** (fitness sharing): slots go to high-tally
sheep, but each pick after the first has its tally discounted by genome
similarity to the already-chosen (variation mix, palette, structure), so one
aesthetic cannot monopolize a generation no matter how many peers vote for
near-clones — the monoculture defense for large swarms. Unfilled slots top up
from the unvoted living (newest first) so the population stays breedable.

At generation close, survivors breed deterministically:

```
for each (a, b) in deterministic_pairing(survivors):
    child = mutate(crossover(a, b), rng = H("breed" ‖ gen ‖ a.id ‖ b.id))
for each s in top-2 survivors:                     # variance injection
    mutant = mutate(s, rate = 0.4, rng = H("mutant" ‖ gen ‖ s.id))
immigrant = random_genome(seed = H("immigrant" ‖ gen))   # fresh blood, forever
```

Because crossover/mutation use the deterministic `flame-core` RNG seeded only
by public data, bred children carry no signature — their legitimacy is checked
by recomputation, not authority.

New blood: any user may also submit a hand-tuned, random, or
breeding-lab-discovered genome as a signed gen-0 sheep. Releasing costs a
render proof (same chunk-hash scheme as votes, challenge bound to the author
and generation — and it doubles as the author's vote), with a deterministic
per-(author, generation) cap.

### The future is previewable (breeding lab)

A consequence of deterministic breeding worth designing the UI around: the
canonical child of **any** pair in the current generation is already fully
determined by public data — before the generation closes, before anyone votes.
The browser can render it on demand:

- Pick any two living sheep → see *the exact child that will be born* if both
  survive this generation. Not a simulation: that genome, that seed.
- Explore **what-if siblings** under alternate seeds
  (`H("whatif" ‖ gen ‖ a.id ‖ b.id ‖ i)`): never canonical, but a beautiful
  one can be submitted as a gen-0 sheep.
- A pairwise matrix over the current top-K shows the entire possible next
  generation at thumbnail quality.

This changes what voting *means*: you're not only judging a sheep, you're
steering toward the offspring you've already seen. Strategy and taste in one
mechanic, and it costs the network nothing — it's all local rendering of
public data.

## Network

The protocol logic is written against a minimal transport interface
(`send(msg)` / `onMessage(fn)` — see `web/js/net.js`), with two
implementations:

- **BroadcastChannel (dev, implemented):** every same-origin tab joins the
  bus, so two tabs are a real two-peer network with no infrastructure.
  `?peer=N` namespaces identity + store per tab so tabs are *distinct* peers.
- **js-libp2p (production, planned):** WebSocket (browser ↔ relay), WebRTC
  (browser ↔ browser, signaled via circuit relay v2). Browsers cannot accept
  inbound connections; the relay exists solely to introduce peers, after which
  data flows direct.
- **Pubsub:** gossipsub topics — `sheep/v1/sheep`, `sheep/v1/votes`,
  `sheep/v1/fraud`. Messages are gossip-friendly sizes (genomes ~3 KB, votes
  ~2.5 KB). Blocks are *not* gossiped as objects — only block hashes appear,
  inside sync exchanges, because blocks are recomputable.
- **Sync on connect:** gossip only reaches peers who are online when a message
  is published, so peers also run an anti-entropy exchange when they meet:
  compare chain-tip block hashes; on mismatch, walk back to the fork point,
  then exchange per-generation vote/sheep inventories and fetch what's
  missing. (Implemented today on the dev transport: each `inv` beacon carries
  one sorted-key digest per (kind, generation) bucket; only mismatched
  buckets exchange keys, then records — O(generations) beacon size instead of
  O(records).) Every browser persists everything
  it has seen in IndexedDB and serves it onward — **every visitor is a
  store-and-forward node.**
- **Ingest validation (before storing or re-gossiping):** signature valid,
  genome parses and is within parameter bounds, vote references a known sheep
  and an unfinalized generation, dedup keys unique, per-peer rate limits.
  Invalid messages are dropped, not propagated.

## Tallying and local trust

A client's displayed vote counts are **its own audited view**, not a global
truth:

1. Collect valid votes per sheep (dedup, bounds, signature).
2. Continuously audit a random sample in a background worker (target: audit
   fraction `p` of vote-chunks; each audit costs 1/M of a render).
3. Discard all votes from any key with a confirmed fraud proof (own audit or
   verified third-party report).
4. Tally what remains; recompute blocks; follow the heaviest chain.

Views converge because everyone audits the same objective facts and applies
the same fork-choice rule. There is no consensus round; the worst a
disagreement causes is a transient fork, which heaviest-work + finality
resolves.

## Cold start and the anchor peer

The brutal truth of pure-swarm state: **if no two visitors are ever online
together, no state propagates.** Mitigations, in order of how much we like
them:

1. **Baked state.** The static site ships seed genomes and, on each redeploy,
   a snapshot of the chain + sheep known at deploy time. A visitor to an empty
   swarm still gets a full gallery — the site is read-usable with zero peers.
2. **Anchor peer.** The relay host (which must exist anyway for WebRTC
   signaling) also runs an ordinary peer with disk persistence: it subscribes,
   stores, and serves anti-entropy sync like any browser would, just with
   100% uptime. It has **no special authority** — it's a peer that happens to
   never sleep, on hardware we own (no ToS issues). The protocol works without
   it; UX is dramatically better with it.
3. **Long-lived tabs.** Enthusiasts leaving the site open (it's a screensaver
   at heart) are organic anchor peers.

## GUI

- **Flock view:** gallery of current-generation sheep, rendered locally and
  progressively (chunks appearing = render status). Pre-tonemapped placeholder
  until first chunks land.
- **Sheep view:** full-quality render, animation (camera spin / genome-space
  loops — free, it's all local), lineage back through the chain, vote button.
- **Voting flow:** "Vote" starts your proof render; the progress bar *is* the
  chunk hashing. On completion the vote signs and publishes. (You watched it;
  that's the point.)
- **Breeding lab:** pick any two living sheep → the canonical child they'd
  have this generation, plus what-if siblings; pairwise matrix over the
  current top-K. "Submit as new sheep" for what-if discoveries.
- **Chain view:** generations as a timeline — survivors, tallies, children,
  forks/reorgs if any. The lineage graph of every sheep back to gen 0.
- **Network panel:** peer count, sync status, chain tip vs. peers', audit
  activity ("verified 14 votes this session, 0 frauds").

All rendering runs in a Web Worker pool; the main thread never blocks.

## What the WASM API needs (delta from today)

Current API is a single synchronous `render_rgba(genome_json, …)`. Needed:

- `render_chunk(genome, seed, chunk_idx, opts) -> (histogram, hash)` — the
  unit of both progressive display and proofs.
- `tonemap(histogram_sum, genome, opts) -> rgba` — re-runnable as chunks
  accumulate.
- `audit_chunk(genome, challenge_seed, chunk_idx, opts) -> hash` — same as
  render_chunk minus the pixels.
- `canonical_sheep_id(genome_json) -> hash`.
- `crossover(a, b, seed)` / `mutate(genome, seed)` — port the breeding
  operators into `flame-core` (deterministic; they're also what the breeding
  lab calls locally).

## Known limits and open questions

- **Float determinism (prerequisite #1).** Byte-identical native/wasm output
  is claimed but was never verified browser-vs-native. Transcendentals
  (`sin_cos`, `ln`, `powf`) may differ by ULPs between system libm and wasm.
  Fix if needed: route all transcendentals through the `libm` crate on every
  target. **Everything in the trust layer depends on this. Verify first.**
  Corollary: any future change to `rng.rs`/iteration order is a breaking
  protocol change (version the proof format).
- **Canonical genome JSON.** Content-addressing needs one canonical byte form
  (sorted keys, fixed float formatting — e.g. JCS / RFC 8785). Define before
  any sheep_id exists in the wild.
- **Sybil floor.** One vote ≈ one render ≈ seconds of CPU. A determined
  attacker with a native build farms votes at maybe 10× browser speed; with
  the chain rule this is also the cost of winning fork choice (it's
  heaviest-*work*, and work can be farmed). This bounds, not eliminates,
  manipulation — acceptable for art, worth saying out loud. Reputation
  weighting (older keys with clean audit history count more) can raise the
  floor later.
- **Clock skew.** Generation boundaries are wall-clock; clients with bad
  clocks sign votes near boundaries into the wrong generation. Accept votes
  for gen G until G+1 closes (the finality window already implies this);
  beyond that they're simply invalid.
- **Reorg UX.** A reorg can un-birth children a user was watching. W = 2 keeps
  the window short; the UI should mark the unfinalized generations visually
  (e.g. "provisional") rather than pretend instant finality.
- **Relay SPOF.** One relay = one point of connectivity failure. Cheap
  mitigation: list several relay multiaddrs in the static config; anyone can
  run one (it's stateless and tiny).
- **Gossip flooding.** Per-peer rate limits + validation-before-relay at
  ingest; votes are the only spammable verifiable message and they're
  expensive to make valid.
- **Eclipse attacks.** A peer surrounded by liars sees a fake flock — but
  audits still catch fake *votes* (objective), bred sheep are recomputable
  (objective), and a fake chain must out-weigh the real one in verifiable
  work. The attacker can only hide state, not forge it.
- **Tuning constants.** M=64 chunks, audit fraction p, K survivors, generation
  length (5 min), finality window W=2, proof sample count (browser-render
  budget ~1–2 M samples) — all TBD by experiment; version them in a protocol
  config.

## Build order

1. **Determinism check/fix** (native vs wasm hash equality on a corpus of
   genomes; adopt `libm` if needed). Gate for everything else.
2. **Chunked rendering in `flame-core`** + new wasm API + Web Worker pool +
   progressive gallery UI. Useful standalone — this is the site MVP, fully
   static, no networking.
3. **Breeding operators in Rust** (crossover/mutate, deterministic) + the
   breeding lab as a local playground. Still no networking.
4. **Swarm v1:** js-libp2p (WebSocket + WebRTC + gossipsub), relay deployment,
   sheep/vote gossip, IndexedDB persistence, anti-entropy sync. Votes accepted
   un-audited (proofs published but trusted).
5. **Trust v1:** background audit worker, fraud proofs, local tally
   discipline.
6. **Generations:** block computation, heaviest-work fork choice, finality,
   pruning, chain-hash-based sync.
7. Polish: chain/lineage explorer, history view, network panel, anchor-peer
   persistence.
