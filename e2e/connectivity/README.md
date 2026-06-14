# P2P connectivity integration test

Catches the recurring **"0 peers / clear your browser data or it won't connect"**
regressions BEFORE deploy. One command builds the stack, runs the real web app in
headless Chromium against a real relay behind Caddy (wss), asserts four
properties, tears everything down, and exits `0` (pass) / `1` (fail).

```sh
./e2e/connectivity/run.sh          # or:  npm run test:connectivity
```

Everything runs in Docker — **no Node on the host required.**

## The stack (`compose.yml`)

Mirrors production exactly:

- **relay** — the one libp2p relay (`relay/` image, `src/relay.js`): WebSocket
  server + circuit-relay-v2 + gossipsub backbone.
- **swarm** — Caddy (`Caddyfile`, `tls internal`) serving `web/` over **https**
  and upgrading libp2p **wss → the relay's plain ws**, so headless Chromium gets a
  real secure transport (browsers launch with `--ignore-certificate-errors`).
- **peers** — one headless-Chromium container each (`peer.mjs`, Playwright),
  launched by `run.sh` with a `ROLE`. Each loads the real app pointed at the test
  relay (`?relay=<maddr>&stress` — `?stress` means it takes **no** production
  relay, so test sheep/votes can never leak into the live swarm).

## What it asserts (each fails loudly + exits non-zero)

| # | Check | Guards |
|---|-------|--------|
| **(a)** | **CONNECT** — each peer reaches `window.__libp2p.services.pubsub.getSubscribers('sheep/v2').length > 0` within ~30s | the everyday break |
| **(b)** | **MESH** — the relay's `[stat]` log shows `mesh>0` with the browser peers | the gossipsub **peer-score** regression (behind Caddy every browser shares one IP; default IPColocation/behaviour penalties drove scores negative → relay refused to graft → `subscribers>0 mesh=0` → every browser saw 0 peers). `relay.js` zeroes those weights. |
| **(c)** | **SYNC** — a **late joiner** converges (anti-entropy) to an established producer's `batchSetHash` within ~45s | late-joiner sync |
| **(d)** | **REGRESSION** — two parts (below) | **commit 30e4526**: `net.start()` must run BEFORE `rebuildFlock()`. Pre-fix, the heavy `computeFlock` replay blocked the network — a populated store never reconnected ("clear data or no peers"). |

### Check (d), in two parts

`getSubscribers('sheep/v2')` is **not** sufficient to guard 30e4526: the libp2p
bundle subscribes to the topic in a **background** task, independent of `main()`'s
ordering, so it goes `>0` even while the app shows 0 peers. The real cure is
`net.start()` — it wires the transport into ingestion and starts the anti-entropy
beacon; only then does the app actually *process peer data*
(`net.counts.recv.inv > 0`).

**(d1) ORDERING — the deterministic guard.** The bug is an ordering invariant in
`main()`: `net.start()` must be called **before** `rebuildFlock()`. `run.sh`
asserts exactly that in the source (`web/js/app.js`). It can never silently
regress and pins the fix precisely. *Verified both ways:* it PASSES on the shipped
order and FAILS when `net.start()`/`rebuildFlock()` are swapped.

Why not a wall-clock race? In a clean docker env the synthetic `computeFlock`
replay can't be made *reliably* 30s+, and worse, the replay **saturates the main
thread** — so a driver-side `recv.inv` poll is starved by the very replay under
test, making any timing race flaky. The source check sidesteps both.

**(d2) REJOIN — the functional guard.** Proves the end-to-end *populated-store*
path actually works (pre-fix it never did). `ROLE=regression`:

1. **Phase 1** — loads fresh and seeds IndexedDB with a heavy history: for each of
   `SEED_GENS` (default 800) distinct past generations, 128 batch records (= one
   earned credit that gen) + one vote spending it. Every gen becomes a *selection*
   gen, so `computeFlock` runs survivor selection + WASM breeding per gen — a
   genesis→now replay that takes **tens of seconds** (cheap to write, expensive to
   replay: the real gen-600+ returning-client shape).
2. **Phase 2** — RELOADS with that store on disk (a co-running `beacon` peer
   supplies the inv) and asserts the net layer **eventually goes live**
   (`recv.inv > 0`) within a generous `REG_LIVE_MS` budget (default 120s) — i.e. a
   returning client with a heavy store still rejoins the swarm rather than hanging
   on "0 peers".

Earlier prototyping also confirmed the *timing* discriminates when the replay is
made heavy enough: with the fix, net-live lands ~3.5s while the replay runs
`>30s`; with the order reverted, net-live never arrives within 30s. That race is
real but environment-sensitive, so (d1) is the canonical guard and (d2) the
functional backstop.

## Knobs (env)

- `CONNECT_PEERS` (default 3) — peers for the CONNECT/MESH phase.
- `SEED_GENS` (default 800) — heavy selection gens for the regression replay.
- `CONNECT_MS` (30000), `SYNC_MS` (45000) — per-check timeouts.

## Files

- `run.sh` — orchestrator: build, start relay+Caddy, scrape relay peer id, run
  each role, assert, tear down, exit 0/1.
- `peer.mjs` — the Playwright peer driver (roles: `connect`, `producer`,
  `latejoiner`, `beacon`, `regression`).
- `compose.yml`, `Caddyfile` — the relay + Caddy stack.
- `package.json` — pins `playwright` (installed once into `node_modules/`, which
  the peer containers mount).
