# proof-of-sheep — v4 lifecycle (deterministic births, vote-aged survival)

**Status: IMPLEMENTED (step 1 + step 2). Built in `crates/sheep-node`.**

Implementation notes / v1 simplifications vs the full design below (all noted
inline as refinements, none change the trust model):
- **Parent selection (§2.1)** is convergence-safe rather than backing-weighted at
  confirm time: a lottery child crosses the tile's OWN sheep with a genesis
  founder picked by the tile hash. Popularity still drives breeding *implicitly*
  (live/voted sheep get rendered more → more confirmed tiles → more offspring),
  and the child is a pure function of converged facts so every node derives it
  identically. Backing-weighted parent selection would need a deterministic
  snapshot to stay convergent — deferred.
- **Vote aging (§3)** uses the simple **sliding window** of the last `VOTE_WINDOW`
  votes (the ts-then-voter-then-seq canonical order), not the exponential
  `DECAY^age` weighting or the causal vote-DAG.
- **Membership `M` (§4.1)** sums `w(rep)` over all rep-bearing keys (no active
  window yet), so it grows but doesn't shrink as contributors leave — `n_target`
  growth is bounded by `ln` regardless.
- **Newborn grace (§4.2)** is not implemented: a 0-backing newborn is stillborn
  until it ranks in (it gets room as `n_target` grows / votes shift).
- **No wire bump**: message formats are unchanged; only the engine's survival/
  birth *interpretation* changed, so the two seeds deploy together (force-recreate
  both) and no v3 node lingers.
- **Pruning**: stillborn lottery births accumulate in the flock map (history);
  bounded-history pruning is a follow-up.

Constants live in `engine.rs`: `VOTE_WINDOW`, `N_TARGET_GROWTH`, `MEMBERSHIP_K`,
`BIRTH_THRESHOLD`, `RANDOM_BIRTH_ONE_IN`, `GENESIS_FLOCK_SIZE` (= `N_base`).

---

**Original design proposal (the full contract):** This supersedes the v3 *lifecycle*
only — births, deaths, population, the credit economy. The v3 substrate is
unchanged and assumed: one global swarm of identical nodes, an append-only log
of signed events gossiped + reconciled by anti-entropy, byte-deterministic
`flame-core` renders, browsers as thin REST clients. See `ARCHITECTURE.md`.

This document exists because the v3 lifecycle had a structural fault that caused
real outages: **births were one-shot gossiped events** (`Mint`/`Breed`) and a
node that missed one diverged *permanently* — and a founding seed **auto-minted
fresh founders on every reboot** (`bootstrap_seed_flock` + `maintain_floor`),
churning sheep identity and letting the two seeds drift into two different
flocks. Browsers then submitted renders for sheep a gateway had never learned and
got **HTTP 422**. The fix is not a patch; it is a different shape for the whole
lifecycle.

---

## 0. The one principle

> **The flock is a pure, deterministic function of three convergent logs.**
> Nothing about births, deaths, or population is *gossiped as a fact* or *minted
> by authority* — it is all *re-derived* by every node from logs that
> anti-entropy guarantees converge. The only clock in the system is **the
> community's own vote count.**

The three logs (all already in v3, all signed, all converge via anti-entropy):

1. **Confirmed work** — tiles that have been rendered *and audited* (the existing
   §6 confirmation). The trust boundary; the source of births and of
   reputation.
2. **Votes** — signed backings of a sheep. The source of survival and of
   breeding selection.
3. **Reputation** — log-derived "proof of useful work" standing per key (already
   computed in v3). The source of Sybil-resistant membership weight.

Because the flock is `f(converged logs)`, a node that is behind does not
*diverge* — it *catches up* and recomputes the identical flock. Transient
disagreement during propagation is fine; it heals. This is the property the v3
gossiped-mint model lacked, and it is why this whole design is worth the rewrite.

There is **no minting authority, no `maintain_floor`, no gossiped `Mint`, no
user-triggered `Breed`, and no wall-clock.**

---

## 1. Genesis

A fixed, deterministic **genesis flock of `N_base = 4` sheep**, derived
identically on every node from constant `(minter_key, ts, index)` seeds (extends
the existing `derive_minted_genesis.rs`, which already grandfathers a fixed
genesis mint past the credit check). Every node produces the byte-identical four
founders **without gossiping anything** — so two seeds can never disagree about
the founding flock, which was the v3 divergence source.

Genesis sheep are ordinary sheep thereafter: they live or die by the same rules
as any other. They are *not* immortal and *not* replenished. Their only role is
to give the very first contributors something to render so the swarm can start
(see §6 bootstrap).

---

## 2. Birth — a clock-free lottery over confirmed work

A birth is **not** an event anyone sends. It is *derived* from a confirmed tile:

```
draw = sha256("birth" ‖ sheep_id ‖ frame ‖ idx ‖ pass ‖ tile_hash)
if draw < BIRTH_THRESHOLD:          # rare
    this confirmed tile spawns a new candidate sheep
```

- The trigger is a **confirmed** tile — already audited, already convergent. You
  cannot make a tile win without doing the real, audited render work; the draw is
  a hash of the deterministic render output (`tile_hash`), which no one controls.
- **No `birth_ms`, no timestamp, no sequence number.** This is deliberate: any
  birth *time* the submitter could write is a grinding lever (pick a moment when
  the flock was small, mint on demand, defeat the population cap). Removing time
  from birth removes the lever entirely. Population control lives **only** in the
  survival ranking (§4), never in a birth gate, so a birth never needs to know
  "how big is the flock right now."
- A birth that lands in an already-full flock simply enters the candidate pool
  with zero backing and ranks below the cutoff — harmlessly *stillborn* until/if
  it earns votes. The cap (§4) does all the regulating.

### 2.1 The genome — vote-weighted breeding, not random

The new sheep is **bred**, echoing original Electric Sheep (users vote; the
system breeds the winners — manual parent-picking was never part of ES):

```
parents = two live sheep chosen by recency-weighted backing (§3),
          selected deterministically using tile_hash as the index
child   = crossover+mutate(parentA, parentB)   # existing deterministic breed,
          seeded from tile_hash, through the existing quality priors
```

A small fraction of births (deterministic on `tile_hash`) are instead pure
random/mutation genomes for diversity — again as ES did. Either way the genome
goes through the existing `derive_minted`/`breed` path so it inherits the
auto-framing + variation/palette priors and is as good as a v3 minted sheep
(render quality is a hard requirement — a hash-random genome without priors is
ugly).

---

## 3. Vote aging — the only clock is the vote count

A sheep's standing is **not** its total votes; it is its votes **weighted by
recency**, and recency is measured in **votes, not seconds**:

```
V        = total votes in the converged log         # global monotonic counter
age(v)   = number of votes ordered after v
weight(v)= DECAY ^ age(v)        # or: 1 if v in the last W votes, else 0
backing(sheep) = Σ weight(v) over votes for that sheep
```

Why vote-count and not wall-clock: it makes erosion track **community
activity**. A busy community ages votes fast (keeping a favorite up takes
frequent fresh support — real pressure); a quiet community ages them slowly
(little erosion). When the swarm goes idle the counter stops and the flock
**freezes, preserved** (see §5). This is the activity-driven behavior we want,
and it falls straight out of the clock choice.

**Ordering votes** (to define "after") needs a deterministic order on the
converged vote set. Two options:

- **(v4.0) Order by declared vote `ts`, tiebroken by hash.** The `ts` is used
  only to *order*, never to measure elapsed seconds — still vote-count aging.
  Mildly grindable (post-date your own vote to keep it fresh a little longer),
  but bounded by your vote budget, equal for everyone, and it **cannot mint a
  sheep or move the population cap** — low severity.
- **(later) Causal order via a vote-DAG** — each vote hash-links the recent votes
  it has seen; "after" = the causal future. Fully deterministic, zero timestamp,
  ungrindable. More machinery; adopt only if vote-freshness gaming ever appears.

`HALF_LIFE` (in votes) is the single dial for how hard turnover pushes: short →
favorites need frequent re-voting; long → a favorite coasts on past love.

---

## 4. Survival — top `N_target`, no guaranteed death

```
live flock = the top  N_target  sheep by recency-weighted backing
N_target   = N_base + GROWTH · ln(1 + M)
```

- A sheep is **alive iff it ranks in the top `N_target`**. Death is implicit:
  fall below the cutoff (out-voted, or `N_target` shrank) and it is gone. There
  is **no lifespan and no guaranteed death** — sustained fresh votes hold any
  sheep up indefinitely (the community *can* keep a favorite), while erosion (§3)
  guarantees constant pressure (turnover is the default).
- Liveness is a **stateless ranking**, not a temporal replay — sort all
  candidates by backing, take the top `N_target`. This is what makes the design
  cheap and convergent; there is no births-and-deaths fold to maintain.

### 4.1 Membership scaling `M` (Sybil-resistant, log, capped)

```
M    = Σ_active  w(rep)                       # over contributors active recently
w(r) = r / (r + K)        ∈ [0,1)             # saturating: fresh≈0, proven→1, never >1
```

- `M` is the swarm's **proven** working size — **not** libp2p peer count (which
  is local, non-deterministic, and trivially Sybil'd). A key counts only via
  *confirmed* (audited) work, so a Sybil must do real work per key to register.
- `w(rep)` **saturates at 1**: reputation only ever pulls a key *up toward*
  counting as one member; tenure can never push it past 1, so an aging swarm
  cannot inflate the flock forever (this was a real bug in an earlier draft).
- `N_target = N_base + GROWTH·ln(1+M)` grows **logarithmically**: the first few
  contributors move the flock a lot; after that it takes ~10× the membership to
  add a constant chunk. Bonus: `dN_target/dM = GROWTH/(1+M)` shrinks as the swarm
  grows, so `N_target` is *most* stable exactly when there are the most nodes to
  disagree — which keeps the boundary fuzz (§7) small at scale.
- "Active" window is also **activity-counted** (work-count), not wall-clock, so
  idle = frozen `M` (consistent with §5).

### 4.2 Newborn grace

A new candidate is **guaranteed live for a short grace window** (a small number
of confirmed-work units) so it can be *seen and voted on* before erosion judges
it. Without this, a 0-vote newborn can never break into a full flock and there is
no new blood. With it: new sheep get a real shot, then survive on earned backing
or fade.

---

## 5. Idle behavior — freeze, preserved

Every clock is an activity counter (vote-count for §3, work-count for §4.1), so
**no activity = total freeze**:

- no confirmed work → no births, work-counter stops → `M` frozen → `N_target`
  frozen;
- no votes → `V` frozen → no aging → backing frozen → ranking static.

The live flock sits **exactly as it was, indefinitely**. Nothing is born, nothing
dies. An abandoned gallery is a *preserved artifact*, not a graveyard; when
someone returns and renders/votes, evolution resumes from that snapshot.

Consequence (chosen, intended): **a sheep cannot die of pure neglect** — only by
being out-competed while the community is active. The flock can still reach a
small size through *active* dynamics, but abandonment freezes rather than empties.
(If "fade to empty on abandonment" were ever wanted, the only way is a wall-clock
on vote-aging or on the `M` window — explicitly rejected here for the activity-
clock elegance.)

---

## 6. Bootstrap

At the very first instant there is no work and no votes, so every genesis sheep
has zero backing and `M = 0` → `N_target = N_base = 4`. The top-4 ranking is then
just the four genesis sheep (equal zero backing, tiebroken by `sheep_id`), so the
genesis flock is live and renderable. Contributors render it, earn rep and
credits, cast votes; `M` rises, `N_target` grows past 4, and the lottery starts
spawning bred offspring. `N_base = 4` is the *only* floor-like quantity, and it is
**not minting** (the thing v3 got wrong) — it is "keep the top 4 existing
candidates live," nothing more.

---

## 7. Why it converges, and why it can't be gamed

**Convergence.** `N_target`, `M`, `V`, vote ages, and backing are all pure
functions of the converged logs. Two nodes with the same logs compute the
identical live flock. While gossip propagates, a node that is behind computes a
slightly different ranking *near the cutoff* — a sheep right at the boundary may
be live on one node and not yet on another, which transiently reproduces the v3
422 (a render for a sheep a gateway hasn't ranked live). But unlike v3 this is
**transient and self-healing**: anti-entropy delivers the missing votes/work and
both nodes settle on the identical cutoff. We trade *permanent* divergence for
*rare, boundary-only, self-healing* divergence — and the soft fuzz of recency
weighting plus the `ln` stability (§4.1) keep the boundary thin.

**No grinding.** Births are gated on *confirmed* work and carry *no time* — there
is no `birth_ms` to forge, so no one can place a birth in a favorable
flock-size moment or mint on demand. The only manipulable input is a vote's
declared `ts` used for *ordering* (§3), which is budget-bounded, symmetric, and
cannot mint a sheep or move the cap.

**No authority, no churn.** No node mints; genesis is derived not gossiped;
reboots reproduce the identical genesis and re-derive the identical flock from
the logs. The v3 reboot-churn and seed-divergence simply cannot occur.

---

## 8. What is removed vs v3

- `bootstrap_seed_flock` wall-clock boot mint → **deterministic genesis (§1).**
- `maintain_floor` replacement minting → **deleted** (survival ranking + births
  replace it).
- Gossiped `Mint` for ambient population → **deleted** (lottery births are
  re-derived, never sent).
- User-triggered `Breed` (credit-spent, gossiped) → **deleted**; breeding is now
  the vote-weighted lottery (§2.1). Removing it also removes the last gossiped
  birth event, so there is **no birth message a node can miss.**
- Wall-clock age decay (`DecayParams`) → **deleted**; survival is recency-weighted
  votes (§3–§4), aged by the vote counter.
- The credit economy's breeding sink → repurposed: credits/rep are now **vote
  weight / Sybil-resistance on voting** (you render to earn the right to steer
  evolution).

## 9. Tunables (all protocol constants — every node must agree)

| name | meaning | starting value |
|---|---|---|
| `N_base` | cold-start / floor live count (genesis size) | **4** |
| `GROWTH` | sheep added per `ln(1+M)` of proven membership | tune (~8–12) |
| `K` | rep at which a contributor counts as ~½ a member | tune |
| `BIRTH_THRESHOLD` (`p0`) | per-confirmed-tile birth probability | rare (~1e-3…1e-4) |
| `HALF_LIFE` / `W` | vote-aging half-life (in votes) / window | tune |
| `GRACE` | newborn guaranteed-live window (work units) | small |
| `RANDOM_FRACTION` | share of births that are random vs bred | small |

## 10. Open implementation notes

- **Efficient liveness:** the live set is a top-`N_target` selection over the
  candidate pool by recency-weighted backing — recomputed incrementally as
  votes/work arrive; checkpoint so a node need not rescan the whole vote log per
  update.
- **Candidate pool pruning:** most lottery births are stillborn (0 votes, below
  cutoff); prune long-dead, never-voted candidates from memory (they stay
  re-derivable from the log if ever needed).
- **Anti-entropy must back-fill votes and confirmed tiles reliably** — the whole
  convergence argument rests on the logs converging. (The v3 bug was a *gossiped
  birth* never re-syncing; here there are no birth messages, but votes/work must.)
- **Fraud retraction:** a confirmed tile later disputed/retracted (§6 fraud) that
  had triggered a birth un-derives that birth — rare, heavy path, already special.
- **Migration:** ship in steps — (1) deterministic genesis + delete
  `maintain_floor` (kills today's churn immediately, keeps v3 survival), then
  (2) the lottery births + recency-vote survival, behind a wire-version bump.
