# Backlog

Running list of designed-but-not-yet-built ideas, captured from design
discussions. Ordered roughly by priority within each section. The committed
design lives in ARCHITECTURE.md; this is the "next" list.

## Done (for reference)
- Vote-credit economy: rendering earns fungible, use-it-or-lose-it **credits**;
  spend them to **back** sheep; selection = backing, decoupled from render
  coverage. Back-only, flat (no anti-whale curve — credits = audited CPU is the
  real Sybil/whale defense), deterministic-recompute enforced (`computeBacking`
  caps spend at earned, drops over-budget votes canonically). New `vote` record
  kind + `votes` store, synced via the same anti-entropy buckets.
- Breeding cost raised to 64 tiles **per parent** (128 total) — a real stake.
- Breeding gate (protocol-enforced in gens.js + UI mirror).
- "What is this" page rewrite (4 sections).
- Future-proofing: protocol version field in every record; render spec is
  data (SPEC_SCHEDULE / specForGen, keyed to each sheep's birth gen).
- Contribute to the fuzziest (least-covered) frame; per-frame tile map.
- Manual contribution; decoupled smooth playback + hover-to-animate.

## Enforcement principle

Everything that affects shared outcomes must be **protocol-enforced**, never
client-gated. Two valid enforcement mechanisms:
- **Ingest validation** (net.js): drop-on-failure, every peer applies it.
- **Deterministic recomputation** (gens.js): every peer computes the same
  result from the same facts (self-healing as facts sync). This is how the
  per-author cap, tallies, and selection are enforced.

Anything "client-side only" must be genuinely local (whether *you* spend CPU,
what you display) with no shared consequence.

## Game mechanics

### Vote-credit economy — SHIPPED (see Done). Remaining options if wanted later:
- **Cull (down-votes):** v1 is back-only. A spend-to-cull action is possible but
  invites coordinated griefing; deferred.
- **Persistent credits:** v1 is per-gen use-it-or-lose-it. A saved balance would
  reward long-haul contributors but concentrates early-adopter power; deferred.
- **Unify breeding with credits:** breeding is still gated by raw tiles on the
  parents (64 each), not credit spend. Could fold breeding rights into the one
  credit currency later.

### Hall of Fame
Keep the best sheep of each generation. Largely **emergent**: the generation
chain is deterministic, so each closed gen's top-K by tally is recomputable
from facts — a derived view, no new stored state. Surface as a timeline page.
Their render is preserved as long as their batch records are (pruning would
lose it; pruning isn't implemented).

### Continued contribution to historic sheep
"Allow additional tiles on past winners." **Already supported by the model**: a
sheep's render is the sum of ALL its batches across all time (gen-agnostic), so
anyone can publish more batches for any known sheep and its image keeps
improving. Open question: such late tiles improve the *render* but shouldn't
necessarily mint *votes* for a sheep not in the active flock — treat as a
"polish the masterpiece" mechanic (render-only, no selection effect). Needs a
small rule: tally only counts batches whose gen == the sheep's active gen(s).

## Presence / live UI

### Live co-rendering presence
When viewing/rendering a sheep, show **how many peers are also rendering it**
and the **current vote count**, live. Needs a new ephemeral gossip beacon
(`{kind:'rendering', from, sheepId, ts}`, TTL ~10–15s, not stored); peers count
distinct recent contributors per sheep. Vote count = net.tally (already exists),
just shown live on the sheep view.

### Rank badges #1 #2 #3
Show leaderboard position on cards (and the sheep view). Derivable from the
current-gen tallies (sort, top-3). Mostly UI.

## Renderer / scale (from ARCHITECTURE "Known limits")

- **Raise render resolution.** The spec is now data-driven (SPEC_SCHEDULE +
  specForGen, keyed to a sheep's birth gen), so add a schedule entry with a
  higher `from` gen to give NEW sheep a crisper spec without breaking old
  sheep's tiles. Constraint: keep nFrames constant across specs (sheep.html
  rebinds only resolution). Measure histogram/sync cost first.
- DONE: cheap verified viewing — render-data is gzip'd (lossless) and a viewer
  can ?fetchonly to fetch+sample-verify the accumulated render instead of
  re-rendering every tile. Relay production deploy recipe in relay/deploy/.
- **Pruning + finality** (generation blocks digest) for unbounded-history scale.
- **Hover animation** currently shows preview-quality motion (not the
  accumulated render); optionally prefer accumulated frames where they exist.
