# proof-of-sheep architecture (v2 — coordinator)

**Electric Sheep, reborn.** A central **coordinator** runs the genetic algorithm,
hands out small deterministic render work-units to volunteers, merges their
results into a canonical per-sheep render, encodes the viewable loop as a video,
and serves it. A **static client** (GitHub Pages) displays the merged videos,
lets people contribute render work, and lets them vote.

This replaces the v1 fully-P2P design (libp2p/WebRTC/gossip), which never
connected reliably in browsers. v1 lives in git history and `attic/v1-p2p-client/`.

The guiding principle survives unchanged: **every displayed pixel is a
deterministic render of a coordinator-authored genome.** Work is verifiable;
results are reproducible; the heavy data is a *recipe*, not a payload.

---

## 1. Shape

```
        ┌─────────────────────────── GitHub Pages (static) ───────────────────────┐
        │  client: display merged video · render assigned tiles (WASM) · vote      │
        └───────────────▲───────────────────────────────────────────┬─────────────┘
                        │ GET video / sheep list (cached at CDN)     │ POST results / votes
                ┌───────┴───────── Cloudflare (CDN + DDoS shield) ───┴───────┐
                └───────▲───────────────────────────────────────────┬───────┘
                        │                                            │
        ┌───────────────┴──────────────── Coordinator (VPS) ────────┴─────────────┐
        │  axum/Rust · reuses flame-core natively · runs the GA · assigns work ·   │
        │  ingests + verifies results · merges histograms · ffmpeg encode · SQLite │
        └──────────────────────────────────────────────────────────────────────────┘
```

- **Static client** — GitHub Pages, free, no app logic that needs trust. Talks
  to the coordinator over plain HTTPS (POST results/votes, GET sheep list).
  Pulls videos via the CDN.
- **Coordinator** — the one stateful service. Authoritative over the flock.
- **Shared renderer** — `crates/flame-core` is the *same deterministic code* run
  natively by the coordinator (verification, final tonemap) and as WASM by the
  client (`web/js/{worker,pool}.js`, contribution). Determinism is the contract
  that lets the coordinator trust a client's pixels by spot-re-rendering.

## 2. The render pipeline & its three forms

A sheep exists in three representations that differ by ~1000× (numbers for the
current spec: 384×384, ss=1, 4 channels, u64 counts, 128 frames):

| form | size | role | mergeable? | distributed? |
|---|---|---|---|---|
| **batch log** (recipe: `(frame, idx)` per tile) | ~0.2–9 MB | canonical contribution record | yes (append) | between coordinators only |
| **accumulated histogram** | **~576 MB** | working merge state | yes (additive) | never leaves the coordinator |
| **viewable video** (tonemap → AV1/VP9) | **~1–4 MB** | what people watch | **no** (lossy) | yes, via CDN |

Pipeline is one-directional:
**log → (re-render + sum) → histogram → tonemap → lossy-compress → video.**
You merge at the left; you distribute the right; the 576 MB only materializes
transiently when a sheep needs repainting. Tonemap is nonlinear, so the video is
a dead-end for merging — keep merge state in the count domain.

**Cost follows egress.** Ingress (clients pushing results) is free; egress
(serving video) is the only bill. A ~2 MB video to 10k viewers ≈ 20 GB ≈ $2 on a
cloud, ~free behind the CDN. So "serve the video, never the histogram" is the
whole hosting story.

## 3. Work model

- The coordinator owns the flock. When a client clicks **contribute**, it gets a
  **bundle of small work-units**: each is `(sheepId, genome, frame, idx)` — render
  `spp` samples for that tile and return the histogram contribution + a signed
  record. The coordinator **assigns distinct idxs**, so no two clients ever render
  the same tile (the v1 collision problem is gone by construction).
- **Small units** keep render/audit/honeypot cheap. Their per-record signature
  overhead is fine because **records are transient**: verify → merge the pixels →
  bump a per-contributor tally for credits/reputation → discard the record. The
  canonical state is the histogram + the tallies, not a growing log.
- A bundle also carries **audit tasks** (see §4) so verification is volunteer work
  too. Round-trips are amortized by bundling N tasks per request.

## 4. Trust model

Two tiers. **Ship the thin one; the hardened one is designed and bolts on without
rework when real abuse appears** (the v1 lesson: don't carry weight you don't need
yet — original Electric Sheep had ~no verification because cheating wasn't
profitable).

**Identity:** the existing **Ed25519 keypair** (`web/js/identity.js`), not IP.
Stable across IP changes, reputation attaches to the key. Keypairs are free →
Sybil-cheap, but that's handled by sampling (a fresh key is heavily audited). **IP
is a secondary signal only** — per-IP rate-limit + flag bursts of new keys from
one IP/subnet for extra scrutiny. Never identity (NAT conflates real users,
dynamic IPs wipe reputation, rotation is cheap for attackers).

**Thin (v2.0):** every result is signed; the coordinator **re-renders a random
sample** and checks the hash (reusing `web/js/audit.js` primitives natively),
applies **count conservation** and **subtract-check**, and **bans + invalidates
all work** on a fraud catch. That alone defeats the realistic threat (bored
griefers) and the economic deterrent (lose everything) makes the rest unprofitable.

**Hardened (v2.x, when needed):**
- **Reputation tiers** — a new key is sampled ~100%; sampling decays toward a
  floor as accepted work accumulates. Never 0%. Reputation ≠ spendable credits.
- **Peer-offloaded auditing** — contribute bundles include audit tasks
  (re-render someone else's claimed tile, report hash match). Deterministic render
  ⇒ honest auditors agree ⇒ a liar is outvoted by N independent reputation-weighted
  auditors.
- **Honeypots** — salt the audit stream with planted tasks the coordinator already
  knows the answer to. **Known-bad** (a real-looking tile with a wrong claimed
  hash) catches rubber-stampers — you can't pass it without actually re-rendering.
  **Known-good** catches false accusers. Grading is *free* (the coordinator planted
  the answer; no re-render). Honeypots are indistinguishable from real audits.
- Reputation governs three things: how much *your work* is audited, how much *your
  audit verdict* is trusted, and whether the coordinator **re-renders to verify**
  you (new) vs **trusts your uploaded pixels** (proven) — the latter is what
  actually offloads the coordinator's compute.

End state: volunteers do the rendering **and** the verification; the coordinator's
compute is just assignment + merge + occasional honeypot/meta-audit + encode.

**Content integrity (free property, not a feature):** since every accepted tile is
a verified deterministic render of the genome, the merged pixels can *only*
converge on the genome's image — arbitrary-image injection through tiles is blocked
by construction. The only content vector is the genome, which the coordinator
authors (server-side GA) and can review/remove (it controls the flock). Fractal
flames can't produce photographic content, only abstract/symbolic shapes.

## 5. Genetic algorithm (server-side)

The GA runs in the coordinator — this deletes v1's hardest problem (deterministic
replay-from-genesis, the divergence/checkpoint/gen-boundary mess). The server
**is** the flock: it tallies votes, selects survivors, breeds (crossover +
mutation), injects fresh-blood random immigrants, and stores genomes. Clients
fetch the current flock; nobody replays anything. Trade: the server is
authoritative over creative direction (breeding fairness becomes "trust the
server"); the *render-work* accounting stays trustless. Votes drive selection and
are **rate-limited / reputation-weighted** so evolution can't be brigaded.

## 6. Deployment & resilience

- **Client:** GitHub Pages (static, `web/`), custom domain via `CNAME`. Free.
- **CDN:** **Cloudflare (free tier)** in front of the coordinator — caches the
  videos (kills egress), absorbs DDoS, terminates TLS at the edge. Important for an
  exposed server.
- **Coordinator stack — Rust:** `axum` + `tokio`, **reusing `flame-core` natively**
  for verification + final tonemap (same deterministic code as the client's WASM,
  full native speed). Rust's memory safety matters for a server eating untrusted
  input. `ffmpeg` shells out for the video encode.
- **State:** **SQLite (WAL mode)** holds the small canonical state — genomes,
  sheep, votes, credits, reputation, per-sheep tile tallies, the batch log. The
  576 MB histograms and the videos are **regenerable caches** on disk (reconstruct
  by re-rendering the log; encode on a quality-delta threshold and cache).
- **Process:** Docker `restart: always` behind **Caddy** (already on the VPS;
  auto-HTTPS). Coordinator API is plain HTTP/SSE — no WebSocket, so the v1 h2/wss
  handshake bug can't recur.
- **Resilience across the two droplets** (relay1 / relay2 — repurposed, not
  destroyed): **primary + warm standby.** Because
  the canonical state is small SQLite, replicate it continuously with
  **litestream** (→ standby or object storage); rsync the histogram/video caches (or
  let the standby regenerate them from the replicated log). Failover via a
  **DigitalOcean reserved (floating) IP** reassigned to the standby. Start with one
  solid box + continuous backup; automate failover when it's worth it.
- **Hardening:** input validation, **bounded verification renders** (iteration/time
  cap in flame-core for any untrusted genome — though genomes are server-authored,
  belt-and-suspenders), per-IP/key **rate limiting** (Cloudflare + tower middleware).

## 7. Reuse vs build

- **Reuse as-is:** `crates/flame-core` + `flame-wasm` + `flame-cli` (the renderer),
  `web/js/{worker,pool}.js` (client render harness), `web/js/loop.js` (frame
  player — though display is mostly video now), `web/js/{identity,hash}.js`
  (signing), `web/js/audit.js` (verify primitives — now run natively too), genesis
  genomes + golden hashes, `web/build.sh` (WASM build).
- **Preserve the v1 front-end** — the look and the game design carry over: keep
  `style.css`, the page markup, sheep names, lineage, hall of fame, the gen
  countdown, per-card tile totals, and the **contribute → earn credits → back a
  sheep** loop. The rebuild is "rewire the plumbing under the same skin," not a
  redesign: swap the data/control layer (`net.js`/`gens.js`/`store.js` → a thin
  coordinator HTTP client), and swap local-render display for **video playback**
  (the boomerang/preview-loading machinery retires). The heat-grid/per-frame stats
  stay, now fed by server coverage data. The page markup lives in
  `attic/v1-p2p-client/` as the basis.
- **Breeding becomes propose-a-pairing.** The nursery stays: spend credits to pick
  two parents → the coordinator does the crossover, renders the child, adds it to
  the flock. Keeps the creative agency; the server still owns genome generation
  (also what keeps malicious genomes out). Pure-vote-driven breeding is the
  fallback if user pairing is dropped.
- **Build new:** the Rust coordinator (axum service: assign / ingest / verify /
  merge / encode / GA / vote / serve), and the client's data/control layer.
- **Cribbable for reference:** the GA logic in `attic/.../gens.js`, the audit/
  count-conservation/subtract logic, the FrameLoop display.

## 8. Phasing

1. **Coordinator skeleton** — axum, SQLite, a hardcoded genome; `/assign`,
   `/submit` (render a tile, return it), thin sampled verify, merge, `/video`.
2. **Static client** — display the served video; a contribute button that pulls a
   bundle, renders in the WASM pool, posts results; show credits.
3. **GA + votes** — server-side breeding, a `/vote` endpoint, the flock list.
4. **Resilience** — Cloudflare, standby + litestream, reserved-IP failover.
5. **Harden trust** *(only when abuse appears)* — reputation tiers, peer-audit,
   honeypots.
