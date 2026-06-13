# Swarm stress testing — handoff brief

Self-contained context for running large-scale stress tests of wasm-sheep on
a big machine (written for a fresh Claude Code session on an EPYC 7452 /
256 GB box; humans welcome too).

## Ground rules (inherited from the main session)

- **NEVER `git push`.** A remote may exist; everything stays local unless the
  user explicitly says otherwise.
- **No Node.js on the host.** Everything Node runs in Docker (this harness
  uses `mcr.microsoft.com/playwright:v1.53.0-noble`). Host needs Docker only.
- The repo's tracked docs are README.md and ARCHITECTURE.md. (The main dev
  machine has gitignored scratchpads; they don't travel — this file is the
  stress-testing context.)

## What this project is (one paragraph)

A static website + browser swarm reviving Electric Sheep: fractal flames
("sheep") rendered deterministically in-browser (Rust→WASM), bred by vote.
Votes are proofs of rendering work (64 signed frame hashes of the sheep's
animation loop, ~41M samples); peers audit each other by re-rendering random
frames, and verified fraud bans a key everywhere. Voters' summed render
histograms are fetchable and verifiable (content-addressed by the vote), so
displayed quality accumulates across voters. Generations close every 5
minutes; top-6 by (niched) net tally breed deterministically on every client,
down-votes cull net-negative sheep, and each active generation also derives
2 mutant clones + 1 random immigrant (deterministic, no consensus needed).
Transport today: BroadcastChannel (same-profile pages = a real swarm);
js-libp2p transport exists behind the same interface for the internet phase.

## Running the stress test

```bash
./stress/run.sh                                   # smoke: 12 peers, 3 min
PEERS=300 MINUTES=30 ./stress/run.sh              # real run
PEERS=600 MINUTES=60 RENDER_SLOTS=24 VOTE_RATE=20 BREED_RATE=2 ./stress/run.sh
```

Knobs (env): `PEERS`, `MINUTES`, `WORKERS` (worker pool per peer, default 1),
`RENDER_SLOTS` (max concurrent proof renders — the CPU governor; ~1 slot per
4 host threads is sane), `VOTE_RATE`/`BREED_RATE` (actions per minute,
swarm-wide), `SAMPLE` (peers polled per metrics tick), `SHM` (docker
--shm-size, raise to 8g+ for hundreds of peers), `MEM_LIMIT` (optional docker
--memory cap — set one; this harness exists to find limits, not to OOM the
box. The main dev machine got OOM-killed once already.)

How it works: one Chromium, one profile, N pages of `index.html?peer=sN&workers=W&stress=1`
— same-profile pages share the BroadcastChannel bus, `?peer=` namespaces each
page's identity/IndexedDB, so the tabs ARE the swarm. `?stress=1` exposes
`window.__sheepAct` (voteRandom/breedRandom — full real proofs, no shortcuts)
and `window.__sheepDump` (metrics + a tally fingerprint). The driver schedules
actions under the render semaphore and emits JSONL to `stress/out.jsonl`.

## What to measure / expected findings at scale

Numbers from the design review (the point of the test is to confirm and
quantify these on real hardware):

1. **Inventory cost.** inv is now digest-based (one 16-hex digest per
   (kind, generation) bucket, jittered 4–7 s; mismatched buckets exchange
   keys via addressed `bucket`/`want-items`, then records). This replaced
   full-key-list broadcasts, the projected first failure at scale — the test
   should CONFIRM `sentBytesMed`/`invSentMed` now stay near-flat as votes
   accumulate, and that repair still converges (bounded to 4 bucket repairs
   per inv received).
2. **Store growth.** `votes` per peer never shrinks (pruning designed, not
   implemented — finality via generation blocks lets vote bodies drop).
3. **Sum-serving fan-out.** `want-sum`/`sum-data` ride the broadcast bus;
   popular sheep multiply 4.7 MB messages. Fix on deck: direct streams +
   re-serving by fetchers (sums are content-addressed → trustless CDN).
4. **Convergence.** `distinctTallyViews` in samples should be 1 (or briefly 2
   during gossip); the FINAL record audits every peer after a 60 s settle.
   Any persistent divergence is a protocol bug — investigate before scaling.
5. **Memory.** ~320 MB/peer measured at 10 peers (Chromium + workers + wasm).
   Expect ~600–700 peers max on 256 GB; renderer-process packing
   (`--renderer-process-limit=32`) is already set in swarm.js.
6. **Audit coverage scales with peers** (each audits ~1 vote/8s when idle) —
   `auditsMed` × peers / votes ≈ audits per vote. Should grow with PEERS.

## Suggested campaign

1. `PEERS=50 MINUTES=10` — baseline; confirm convergence + flat failure rates.
2. `PEERS=200 MINUTES=20 VOTE_RATE=10` — inv traffic should now dominate
   `sentBytesMed`; plot it from out.jsonl.
3. `PEERS=500 MINUTES=30 RENDER_SLOTS=12 VOTE_RATE=20` — memory + convergence
   under load; expect sampling timeouts if main threads saturate.
4. Partition test (manual): modify swarm.js to pause N pages (page.evaluate
   stop net? or CDP network conditions) for some minutes, resume, verify
   convergence — anti-entropy's real exam.
5. Report: votes/min sustained, bytes/peer/min by message kind, MB/peer,
   convergence lag, audit coverage — vs PEERS. That data drives the digest-
   sync + pruning + stream-sums work (the "10,000× readiness list").

## Repo orientation

- `web/` static site (no build step; `python3 -m http.server -d web 8000`)
- `crates/` Rust: flame-core (deterministic renderer — NEVER change its
  bitstream casually; golden tests are protocol alarms), flame-wasm bindings
- `e2e/run.sh` full functional suite (Playwright-in-Docker) — run after ANY
  change; `relay/` the libp2p relay (Docker) for the internet phase
- Wire format: bump `CHANNEL` + store names in web/js/{net,store}.js together
  on breaking changes (currently v10)
