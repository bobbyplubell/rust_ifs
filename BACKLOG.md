# Backlog

Running list of designed-but-not-yet-built ideas, captured from design
discussions. Ordered roughly by priority within each section. The committed
design lives in ARCHITECTURE.md; this is the "next" list.

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

### Breeding gate — IN PROGRESS (protocol-enforced)
You may only release a bred child if you (the author) have contributed
≥ `BREED_MIN_TILES` tiles to **both** parents. Enforced in gens.js (a release
is admitted to the flock only if the fact set holds ≥T author-batches on each
parent) AND mirrored as a UI gate for instant feedback. Threshold TBD (~4–8).

### Vote-credit economy (the big open design)
Decouple render-work from voting. Contributing earns a **vote credit**; you
spend credits *manually* on selection (back / cull sheep for next gen). Open
decisions:
- **Credit lifecycle:** per-generation use-it-or-lose-it (recommended — no
  stockpiling/whales) vs persistent balance.
- **For/against:** spend +1 (back) or −1 (cull).
- **Anti-whale shaping:** flat / cap-per-sheep / quadratic cost (recommended —
  escalating cost to pile votes on one sheep; rewards spreading + conviction).
- **What one "contribute" action does:** a toggle that renders tiles while you
  watch, accruing credits ≈ tiles rendered.
- Unify with the breeding gate so contribution is one currency: render work →
  (a) prettier sheep, (b) selection votes, (c) breeding rights.
- Must be protocol-enforced (credits earned only via audited rendering; spends
  validated). Currently selection tally = raw batch count; this replaces it
  with spent-credits.

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

## Docs / onboarding

### "What is this?" page (about.html) — sections
Concise, but the technical ones explain the architecture well:
1. **What is this** — Electric Sheep reborn; community-rendered fractal flames.
2. **How do I play** — view, contribute tiles (render = vote + beautify),
   breed (gated), curate selection.
3. **How does it work** — deterministic renderer, batches as shared tiles,
   integer histograms, the verification gate, generations, no server.
4. **The original Electric Sheep** — Draves' distributed screensaver, render
   farm, genetic voting; what we kept and what's different (p2p, proofs).

## Renderer / scale (from ARCHITECTURE "Known limits")

- **BATCH_SPEC resolution** is 384² for v1 — raise toward a crisp fullscreen
  target (heavier histograms/sync; measure first).
- **Compressed-frame fidelity** for CPU-light viewers: serve tonemapped
  frames / short video (a few MB) so viewers can watch without re-rendering;
  verify by spot re-render. Currently viewers re-render from the ledger.
- **Pruning + finality** (generation blocks digest) for unbounded-history scale.
- **Hover animation** currently shows preview-quality motion (not the
  accumulated render); optionally prefer accumulated frames where they exist.
