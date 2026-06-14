// One headless-Chromium peer driver for the connectivity integration test.
// It loads the REAL web app (web/index.html) pointed at the test relay
// (?relay=<maddr>&stress) and runs ONE of several ROLEs, each of which asserts a
// specific connectivity property and exits non-zero (loudly) on failure. The
// orchestrator (run.sh) wires the roles into the bugs they guard:
//
//   ROLE=connect      CONNECT + MESH: getSubscribers('sheep/v2')>0 within
//                     CONNECT_MS; auto-contributes so it joins the gossip mesh
//                     (the relay's [stat] line must then show mesh>0 — run.sh
//                     asserts that against the relay log). Guards the everyday
//                     "0 peers" break and the relay peer-score regression.
//   ROLE=producer     SYNC (source): connects, contributes a fixed set of tiles
//                     to a specific sheep, prints target id + batch count, stays up.
//   ROLE=latejoiner   SYNC (sink): starts LATER, must replicate the producer's
//                     batch count for that sheep within SYNC_MS via anti-entropy.
//   ROLE=regression   THE bug (commit 30e4526): runs in a PERSISTENT context
//                     (userDataDir), first POPULATES IndexedDB with a heavy store
//                     (many votes across many past gens => a slow genesis→now
//                     computeFlock replay), then RELOADS with that state on disk
//                     and asserts getSubscribers>0 within CONNECT_MS. Pre-fix,
//                     net.start() ran AFTER the replay, so a populated store never
//                     reached the network in time and this FAILS.
//
// No Node on the host: this runs inside the Playwright docker image.

import { chromium } from 'playwright';

const ROLE = process.env.ROLE || 'connect';
const WEB = process.env.WEB_URL || 'https://swarm';
const RELAY = process.env.RELAY_MADDR;
const PEER = process.env.PEER || process.env.HOSTNAME || ('c' + Math.floor(Math.random() * 1e6));
const CONNECT_MS = +(process.env.CONNECT_MS || 30_000);
const SYNC_MS = +(process.env.SYNC_MS || 45_000);
// Generous budget for the returning-client (regression) net-live check: the heavy
// genesis→now replay can hog the main thread for tens of seconds, and the point is
// that a populated store STILL rejoins (not a stopwatch).
const REG_LIVE_MS = +(process.env.REG_LIVE_MS || 120_000);
// Heavy selection gens for the regression replay: enough that rebuildFlock (the
// genesis→now replay) takes several seconds, so the net.start()-vs-replay
// ORDERING is observable (the replay must out-last the ~2-3s anti-entropy inv
// round-trip for the order to be the discriminator).
const SEED_GENS = +(process.env.SEED_GENS || 800);
const TARGET_SHEEP = process.env.TARGET_SHEEP || '';  // sheepId the producer contributes to

const tag = `[${ROLE}:${PEER}]`;
const log = (...a) => console.log(tag, ...a);
const die = (msg) => { log('FAIL:', msg); process.exitCode = 1; throw new Error(msg); };

if (!RELAY) die('RELAY_MADDR not set');

// ?stress so the page takes NO production relay (can't leak into the live swarm);
// ?relay= points it at the test relay. workers=1 keeps idle peers light.
const appUrl = (extra = '') =>
  `${WEB}/index.html?peer=${PEER}&stress=1&workers=1&relay=${encodeURIComponent(RELAY)}${extra}`;

const launchArgs = ['--ignore-certificate-errors', '--enable-experimental-web-platform-features',
  '--no-sandbox', '--disable-gpu'];

// Poll getSubscribers('sheep/v2') until >0 or timeout. Returns ms-to-connect.
async function waitConnected(page, budgetMs) {
  const start = Date.now();
  await page.waitForFunction(
    () => {
      const n = window.__libp2p;
      try { return !!n && n.services.pubsub.getSubscribers('sheep/v2').length > 0; }
      catch { return false; }
    },
    { timeout: budgetMs, polling: 250 },
  ).catch(() => die(`getSubscribers('sheep/v2') stayed 0 for ${budgetMs}ms — peer never connected`));
  return Date.now() - start;
}

// Poll until the app's NET LAYER is live — i.e. net.start() has run AND it is
// processing peer traffic (an anti-entropy inv was received). This is the signal
// the "clear data or no peers" regression actually breaks: libp2p connects in
// the BACKGROUND (so getSubscribers can go >0 on its own), but net.start() —
// which wires the transport into ingestion and starts the inv beacon — was
// blocked behind the heavy genesis→now replay pre-fix, so the app received NO
// peer data and showed 0 peers. Requires a co-present beacon peer (run.sh keeps
// one alive). Returns ms-to-live.
async function waitNetLive(page, budgetMs) {
  const start = Date.now();
  await page.waitForFunction(
    async () => {
      if (!window.__sheepDump) return false;
      try { const d = await window.__sheepDump(); return (d.net?.recv?.inv || 0) > 0; }
      catch { return false; }
    },
    { timeout: budgetMs, polling: 500 },
  ).catch(() => die(`net layer never went live (recv.inv stayed 0) in ${budgetMs}ms — ` +
    `net.start() blocked behind the flock replay? (the "0 peers" regression)`));
  return Date.now() - start;
}

// A long-lived peer that just stays connected and beacons anti-entropy invs, so
// another peer can observe its NET layer going live. Contributes a little so it
// holds a live subscription (=> in the relay mesh too).
async function roleBeacon(browser) {
  const ctx = await browser.newContext({ ignoreHTTPSErrors: true });
  const page = await ctx.newPage();
  await page.goto(appUrl(), { waitUntil: 'domcontentloaded', timeout: 60_000 });
  await page.waitForFunction(() => !!window.__sheepAct, { timeout: 60_000 });
  await waitConnected(page, CONNECT_MS);
  log('beacon up');
  const end = Date.now() + +(process.env.HOLD_MS || 90_000);
  while (Date.now() < end) {
    await page.evaluate(() => window.__sheepAct.contributeRandom()).catch(() => {});
    await new Promise((r) => setTimeout(r, 2000));
  }
  log('beacon done');
  await browser.close();
}

const firstSheepId = (page) =>
  page.evaluate(() => document.querySelector('.card')?.dataset.id || null);
// How many batches we hold for a given sheep — the positive, monotonic
// convergence signal for the SYNC check (a whole-store hash is a moving target:
// stress peers contribute in the background and gossip cross-pollinates, so the
// store sizes legitimately differ between peers; the PRODUCER's tiles for ITS
// target sheep, though, must replicate to a late joiner).
const batchesFor = (page, sheepId) =>
  page.evaluate((id) => new Promise((res) => {
    const ns = (new URLSearchParams(location.search).get('peer')) || '0';
    const r = indexedDB.open(`sheep-store-v16-${ns}`, 1);
    r.onsuccess = () => {
      const all = r.result.transaction('batches').objectStore('batches').getAll();
      all.onsuccess = () => res(all.result.filter((b) => b.sheepId === id).length);
    };
  }), sheepId);

// ---------------------------------------------------------------------------

async function roleConnect(browser) {
  const ctx = await browser.newContext({ ignoreHTTPSErrors: true });
  const page = await ctx.newPage();
  page.on('pageerror', (e) => log('pageerror', e.message.slice(0, 160)));
  await page.goto(appUrl(), { waitUntil: 'domcontentloaded', timeout: 60_000 });
  await page.waitForFunction(() => !!window.__sheepAct, { timeout: 60_000 });
  const ms = await waitConnected(page, CONNECT_MS);
  log(`CONNECT ok: getSubscribers>0 in ${ms}ms`);
  // Also assert the NET layer goes live (received an inv from a peer) — the
  // everyday "0 peers" break. Needs a sibling: with CONNECT_PEERS>1 the other
  // connect peers are each other's beacon.
  const liveMs = await waitNetLive(page, CONNECT_MS);
  log(`NET live: received anti-entropy inv in ${liveMs}ms`);
  // Stay up and contribute so we keep a live subscription to the data topic —
  // this is what puts us in the relay's gossip mesh, which run.sh asserts via the
  // relay's [stat] mesh>0 line.
  const end = Date.now() + +(process.env.HOLD_MS || 40_000);
  while (Date.now() < end) {
    await page.evaluate(() => window.__sheepAct.contributeRandom()).catch(() => {});
    await new Promise((r) => setTimeout(r, 1500));
  }
  log('done holding');
  await browser.close();
}

async function roleProducer(browser) {
  // ?nocontribute: no background auto-contribute, so the producer's store is
  // DETERMINISTIC (just the tiles it explicitly contributes below) — the late
  // joiner can then converge on an exact, non-moving target count.
  const ctx = await browser.newContext({ ignoreHTTPSErrors: true });
  const page = await ctx.newPage();
  await page.goto(appUrl('&nocontribute'), { waitUntil: 'domcontentloaded', timeout: 60_000 });
  await page.waitForFunction(
    () => window.__sheepAct && document.querySelectorAll('.card').length > 0,
    { timeout: 60_000 });
  await waitConnected(page, CONNECT_MS);
  // Contribute a fixed number of tiles to ONE sheep — a concrete fact set a late
  // joiner must replicate. (contributeTo publishes a signed batch each call.)
  const sheepId = TARGET_SHEEP || (await firstSheepId(page));
  for (let i = 0; i < 8; i++) {
    await page.evaluate((id) => window.__sheepAct.contributeTo(id), sheepId).catch(() => {});
    await new Promise((r) => setTimeout(r, 400));
  }
  await new Promise((r) => setTimeout(r, 1500));
  const count = await batchesFor(page, sheepId);
  log(`PRODUCER ready target=${sheepId} count=${count}`);
  // Stay up the whole window so anti-entropy has a peer to converge against.
  await new Promise((r) => setTimeout(r, SYNC_MS + 15_000));
  await browser.close();
}

async function roleLatejoiner(browser) {
  const sheepId = process.env.TARGET_SHEEP;
  const want = +(process.env.WANT_COUNT || 0);
  if (!sheepId || !want) die('TARGET_SHEEP / WANT_COUNT not set for latejoiner');
  const ctx = await browser.newContext({ ignoreHTTPSErrors: true });
  const page = await ctx.newPage();
  // Late joiner does NOT contribute — so any batches it holds for the target
  // sheep came purely from anti-entropy replication of the producer's facts.
  await page.goto(appUrl('&nocontribute'), { waitUntil: 'domcontentloaded', timeout: 60_000 });
  await page.waitForFunction(() => !!window.__sheepAct, { timeout: 60_000 });
  await waitConnected(page, CONNECT_MS);
  log(`connected; awaiting anti-entropy replication of ${want} batches for ${sheepId.slice(0, 8)}`);
  const start = Date.now();
  let have = 0;
  while (Date.now() - start < SYNC_MS) {
    have = await batchesFor(page, sheepId);
    if (have >= want) {
      log(`SYNC ok: replicated ${have}/${want} producer batches in ${Date.now() - start}ms`);
      await browser.close();
      return;
    }
    await new Promise((r) => setTimeout(r, 1000));
  }
  die(`did not converge: have ${have}/${want} batches for ${sheepId} after ${SYNC_MS}ms`);
}

// THE regression. Persistent context so IndexedDB survives the reload.
async function roleRegression() {
  const userDataDir = process.env.USER_DATA_DIR || '/tmp/regression-profile';
  // --- Phase 1: load fresh, then POPULATE the store with a heavy history. ---
  // computeFlock replays every generation that has a vote/submission and BREEDS
  // (WASM) per gen, so votes spread across many distinct PAST gens make the
  // genesis→now replay slow — exactly the populated-store condition that, pre-fix
  // (net.start() after rebuildFlock), starved the network on a returning client.
  {
    const ctx = await chromium.launchPersistentContext(userDataDir, {
      args: launchArgs, ignoreHTTPSErrors: true,
    });
    const page = await ctx.newPage();
    page.on('pageerror', (e) => log('pageerror', e.message.slice(0, 160)));
    await page.goto(appUrl(), { waitUntil: 'domcontentloaded', timeout: 60_000 });
    await page.waitForFunction(
      () => window.__sheepAct && document.querySelectorAll('.card').length > 0,
      { timeout: 60_000 });
    const seeded = await page.evaluate(async (gens) => {
      // Seed directly into the app's IndexedDB (same db name the app opens), to
      // manufacture the heavy genesis→now replay that a real returning client (gen
      // 600+) faces. computeFlock(gens.js) is slow precisely on a store with many
      // SELECTION gens: each gen that has BACKED votes runs survivor selection and
      // BREEDS the survivors via the WASM breedFn — and those awaited WASM breeds,
      // serially over hundreds of gens, are the cost that pre-fix blocked
      // net.start() behind. So for each of `gens` distinct PAST generations we seed
      //   - 128 batch records by one contributor on a baked sheep (= 1 earned
      //     credit that gen; earnedByGen just COUNTS records, no validation), and
      //   - 1 vote spending that credit (=> backing>0 => selection => breeding).
      // Cheap to write, expensive to replay — exactly the populated-store shape.
      const ns = (new URLSearchParams(location.search).get('peer')) || '0';
      const db = await new Promise((res, rej) => {
        const r = indexedDB.open(`sheep-store-v16-${ns}`, 1);
        r.onsuccess = () => res(r.result); r.onerror = () => rej(r.error);
      });
      const ids = [...document.querySelectorAll('.card')].map((c) => c.dataset.id);
      const sheepId = ids[0];
      const voter = 'seedvoter000000000000000000000000000000000000000000000000000000000';
      const GEN_MS = 300_000;
      const now = Math.floor(Date.now() / GEN_MS);
      const batches = [], votes = [];
      for (let i = 0; i < gens; i++) {
        const g = now - 1 - i;                    // distinct past gens, newest backward
        for (let t = 0; t < 128; t++) {           // 128 tiles => 1 credit this gen
          batches.push({ key: `sb-${g}-${t}`, sheepId, frame: 0, idx: t, gen: g,
            contributor: voter, spp: 1, count: 1, hash: 'x' });
        }
        votes.push({ key: `sv-${g}`, from: voter, gen: g, sheepId, n: 1, seq: 0 });
      }
      const put = (store, recs) => new Promise((res, rej) => {
        const tx = db.transaction(store, 'readwrite');
        const os = tx.objectStore(store);
        for (const r of recs) os.put(r);
        tx.oncomplete = () => res(); tx.onerror = () => rej(tx.error);
      });
      await put('batches', batches);
      await put('votes', votes);
      return { gens, batches: batches.length, votes: votes.length };
    }, SEED_GENS);
    log(`populated store: ${seeded.batches} batches + ${seeded.votes} votes ` +
        `across ${seeded.gens} selection gens (heavy genesis→now replay)`);
    await ctx.close();
  }

  // --- Phase 2: RELOAD with that populated store on disk. This is the returning
  // client. Pre-fix it would replay the whole heavy flock BEFORE net.start() and
  // never connect in time; post-fix net.start() runs first and it connects fast.
  {
    const ctx = await chromium.launchPersistentContext(userDataDir, {
      args: launchArgs, ignoreHTTPSErrors: true,
    });
    const page = await ctx.newPage();
    page.on('pageerror', (e) => log('pageerror', e.message.slice(0, 160)));
    const t0 = Date.now();
    await page.goto(appUrl(), { waitUntil: 'domcontentloaded', timeout: 60_000 });
    // LIVENESS, generous budget (REG_LIVE_MS): the returning client — with a heavy
    // store on disk — must still EVENTUALLY process a peer's anti-entropy inv
    // (recv.inv>0), i.e. it actually rejoins the swarm rather than getting stuck on
    // "0 peers". The heavy genesis→now replay can saturate the main thread for tens
    // of seconds, so this budget exceeds it on purpose: the point is "a populated
    // store still converges", not a stopwatch. (A co-running beacon peer, started by
    // run.sh, supplies the inv.) The net.start()-BEFORE-replay ORDERING — the actual
    // shape of commit 30e4526 — is asserted deterministically by run.sh's source
    // check; here we prove the end-to-end populated-store path is not broken.
    const ms = await waitNetLive(page, REG_LIVE_MS);
    // Sanity: confirm the store really was heavy (the bug only bites a populated
    // store; a test that silently lost its seed would pass vacuously).
    const batchCount = await page.evaluate(async () => {
      const ns = (new URLSearchParams(location.search).get('peer')) || '0';
      const db = await new Promise((res) => {
        const r = indexedDB.open(`sheep-store-v16-${ns}`, 1); r.onsuccess = () => res(r.result);
      });
      return new Promise((res) => {
        const rq = db.transaction('batches').objectStore('batches').count();
        rq.onsuccess = () => res(rq.result);
      });
    });
    if (batchCount < SEED_GENS * 128) die(`store not persisted across reload: ${batchCount} batches (< ${SEED_GENS * 128})`);
    log(`REGRESSION ok: returning client with a ${batchCount}-batch / ${SEED_GENS}-gen store ` +
        `rejoined the swarm — net layer went live (peer inv received) in ${ms}ms ` +
        `(wall=${Date.now() - t0}ms) despite the heavy genesis→now replay`);
    await ctx.close();
  }
}

// ---------------------------------------------------------------------------

const roles = {
  connect: roleConnect, producer: roleProducer, beacon: roleBeacon,
  latejoiner: roleLatejoiner, regression: roleRegression,
};
const run = roles[ROLE];
if (!run) die(`unknown ROLE ${ROLE}`);

try {
  if (ROLE === 'regression') {
    await roleRegression();
  } else {
    const browser = await chromium.launch({ args: launchArgs });
    await run(browser);
  }
  log('PASS');
  process.exit(process.exitCode || 0);
} catch (e) {
  log('ERROR', e?.message || e);
  process.exit(1);
}
