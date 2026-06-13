// End-to-end stack test (batch / community-render era), run in Docker (run.sh).
// Drives real Chromium:
//   1. determinism: browser render_batch is byte-identical to the native golden.
//   2. two peers contribute batches and the work syncs both ways (the shared
//      sheep grows on both); tallies reflect it.
//   3. the verification gate rejects a forged render (bytes that don't match
//      the claimed batches) — the headline security property.
//   4. a forged batch (wrong hash) is caught by the auditor and the key banned.
//   5. the swarm page reflects contributions and the ban.
//   6. the generation engine breeds children from batch tallies.

import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { extname, join, normalize } from 'node:path';
import { chromium } from 'playwright';

const WEB = new URL('../web/', import.meta.url).pathname;
const MIME = {
  '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css',
  '.json': 'application/json', '.wasm': 'application/wasm', '.txt': 'text/plain',
};
const server = createServer(async (req, res) => {
  try {
    const path = normalize(decodeURIComponent(new URL(req.url, 'http://x').pathname));
    const file = join(WEB, path === '/' ? 'index.html' : path);
    if (!file.startsWith(WEB)) throw new Error('traversal');
    const body = await readFile(file);
    res.writeHead(200, { 'content-type': MIME[extname(file)] || 'application/octet-stream' });
    res.end(body);
  } catch { res.writeHead(404); res.end(); }
});
await new Promise((r) => server.listen(0, '127.0.0.1', r));
const base = `http://127.0.0.1:${server.address().port}`;
console.log('serving web/ at', base);

const browser = await chromium.launch({
  args: ['--enable-experimental-web-platform-features'],
});
const ctx = await browser.newContext();
ctx.setDefaultTimeout(120_000);

let failures = 0;
const ts = () => new Date().toISOString().slice(11, 19);
const section = (name) => console.log(`[${ts()}] === ${name}`);
const check = (name, ok, extra = '') => {
  console.log(`[${ts()}] ${ok ? 'PASS' : 'FAIL'}  ${name}${extra ? ' — ' + extra : ''}`);
  if (!ok) failures++;
};
ctx.on('weberror', (e) => console.log('PAGE ERROR:', e.error().message));

// ---- 1. determinism: render_batch browser == native golden ------------------
{
  section('determinism');
  const page = await ctx.newPage();
  page.on('console', (m) => { if (m.type() === 'error') console.log('console.error:', m.text()); });
  await page.goto(`${base}/about.html`);
  const out = await page.evaluate(async () => {
    const w = await import('./pkg/flame_wasm.js');
    await w.default();
    const g = w.random_genome_json(2, 3);
    const id = w.sheep_id(g);
    const a = w.render_batch(g, id, 2, 5, 64, 64, 1, 50000);
    const b = w.render_batch(g, id, 2, 5, 64, 64, 1, 50000);
    return { a: a.hash, b: b.hash, cells: a.hist.length };
  });
  const GOLDEN = 'fdd630454bbe7cc3475daf9d9e3ef55bc2b183d93ac7698b9b32cd0a6d37ac15';
  check('render_batch byte-identical browser vs native', out.a === GOLDEN && out.a === out.b,
    `${out.a.slice(0, 16)}… (golden ${GOLDEN.slice(0, 16)}…), cells=${out.cells}`);
  await page.close();
}

// ---- 2/3. two peers contribute + verify gate --------------------------------
let firstId; // a baked sheep id, reused by the swarm section's peer=1 store
{
  section('two peers: contribute + sync');
  const p1 = await ctx.newPage();
  const p2 = await ctx.newPage();
  for (const [n, p] of [['p1', p1], ['p2', p2]]) {
    p.on('console', (m) => { if (m.type() === 'error') console.log(`${n} console.error:`, m.text()); });
    p.on('pageerror', (e) => console.log(`${n} pageerror:`, e.message));
  }
  await p1.goto(`${base}/index.html?peer=1&stress=1`);
  await p2.goto(`${base}/index.html?peer=2&stress=1`);

  await p1.locator('.card').first().waitFor();
  await p2.locator('.card').first().waitFor();
  const cards1 = await p1.locator('.card').count();
  check('p1 flock has cards', cards1 >= 6, `${cards1} cards`);

  await p1.locator('#status', { hasText: /[1-9] peers/ }).waitFor({ timeout: 30_000 });
  check('p1 sees a peer', true);

  // First card renders non-black, full-coverage (preview/accumulation).
  await p1.waitForFunction(() => {
    const c = document.querySelector('.card canvas');
    if (!c) return false;
    const d = c.getContext('2d').getImageData(0, 0, c.width, c.height).data;
    for (let i = 0; i < d.length; i += 4) if (d[i] + d[i + 1] + d[i + 2] > 30) return true;
    return false;
  }, undefined, { timeout: 120_000 });
  check('p1 first card rendered pixels', true);

  await p1.waitForFunction(() => !!window.__sheepAct);
  await p2.waitForFunction(() => !!window.__sheepAct);
  const pub1 = (await p1.evaluate(() => window.__sheepDump())).pub;

  // p1 contributes a few batches (each ~one render); they earn votes and grow
  // the shared sheep.
  const contributed = [];
  for (let i = 0; i < 4; i++) {
    const id = await p1.evaluate(() => window.__sheepAct.contributeRandom());
    if (id) contributed.push(id);
  }
  check('p1 contributed batches', contributed.length >= 3, `${contributed.length} batches`);

  // p2 must receive p1's batches over the gossip bus (the shared sheep grows on
  // p2 without p2 rendering them).
  let got = false;
  for (let t = 0; t < 40 && !got; t++) {
    got = await p2.evaluate(async (pub1) => {
      const { openStore } = await import('./js/store.js');
      const s = await openStore();
      return (await s.allBatches()).some((b) => b.contributor === pub1);
    }, pub1);
    if (!got) await p2.waitForTimeout(1000);
  }
  check("p2 received p1's contributed batches (work synced)", got);

  // A contributed sheep's tally on p2 reflects p1's work.
  const tally = await p2.evaluate(async (sid) => {
    const { gen } = await import('./js/net.js');
    const { openStore } = await import('./js/store.js');
    const s = await openStore();
    return (await s.allBatches()).filter((b) => b.sheepId === sid && b.gen === gen()).length;
  }, contributed[0]);
  check('p2 tally reflects contributions', tally >= 1, `tally=${tally}`);

  // VERIFICATION GATE: a forged render — an all-zero histogram claiming to
  // contain a real batch — must be rejected.
  firstId = await p1.locator('.card').first().getAttribute('data-id');
  const gate = await p1.evaluate(async (sid) => {
    const { BATCH_SPEC } = await import('./js/net.js');
    const cells = BATCH_SPEC.width * BATCH_SPEC.ss * BATCH_SPEC.height * BATCH_SPEC.ss * 4;
    // Ensure batch sid:0:0 exists locally so the claim references a real record.
    await window.__sheepAct.contributeRandom();
    const zero = new BigUint64Array(cells); // contains NOTHING
    const forged = await window.__sheepVerify({
      sheepId: sid, frame: 0, hist: zero, batchKeys: [`${sid}:0:0`],
    });
    return { forged };
  }, firstId);
  check('verify gate REJECTS a forged render', gate.forged === false,
    `verifyRender returned ${gate.forged}`);

  await p1.close();
  await p2.close();
}

// ---- 4. fraud: forged batch caught by the auditor, key banned ---------------
{
  section('fraud injection');
  // A pure auditor peer (no contribution → free pool, fast audits). It already
  // holds the baked flock, so a forged batch for a baked sheep is auditable.
  const page = await ctx.newPage();
  page.on('console', (m) => { if (m.type() === 'error') console.log('audit console.error:', m.text()); });
  page.on('pageerror', (e) => console.log('audit pageerror:', e.message));
  await page.goto(`${base}/index.html?peer=1&stress=1&nocontribute=1&auditms=2000`);
  await page.locator('.card').first().waitFor();
  await page.waitForFunction(() => !!window.__sheepAct);

  const forger = await page.evaluate(async (sid) => {
    const { batchSignBytes, batchKey, gen, CHANNEL } = await import('./js/net.js');
    const { hex } = await import('./js/hash.js');
    const pair = await crypto.subtle.generateKey({ name: 'Ed25519' }, false, ['sign', 'verify']);
    const contributor = hex(new Uint8Array(await crypto.subtle.exportKey('raw', pair.publicKey)));
    const record = {
      sheepId: sid, frame: 0, idx: 99, hash: 'de'.repeat(32), // plausible but WRONG hash
      spp: 640000, contributor, gen: gen(),
    };
    record.sig = hex(new Uint8Array(
      await crypto.subtle.sign({ name: 'Ed25519' }, pair.privateKey, batchSignBytes(record))));
    record.key = batchKey(record);
    new BroadcastChannel(CHANNEL).postMessage({ kind: 'batch', record });
    return contributor;
  }, firstId);

  let banned = false;
  let stats = {};
  const t0 = Date.now();
  while (Date.now() - t0 < 120_000) {
    stats = await page.evaluate(() => ({
      audits: window.__sheepStats.audits, frauds: window.__sheepStats.frauds,
      banned: window.__sheepStats.banned,
    }));
    if (stats.banned.includes(forger)) { banned = true; break; }
    console.log(`[${ts()}] fraud poll: audits=${stats.audits} frauds=${stats.frauds}`);
    await page.waitForTimeout(2500);
  }
  check('forged batch convicted and key banned', banned,
    `audits=${stats.audits} frauds=${stats.frauds}`);
  await page.close();
}

// ---- 5. swarm status page reflects contributions + ban ----------------------
{
  section('swarm page');
  const page = await ctx.newPage();
  page.on('console', (m) => { if (m.type() === 'error') console.log('swarm console.error:', m.text()); });
  await page.goto(`${base}/swarm.html?peer=1`);
  // Wait for refresh() to finish (it runs computeFlock + breeds) — the totals
  // placeholder is replaced only on completion.
  await page.waitForFunction(
    () => document.querySelector('#totals')?.textContent.includes('batches contributing'),
    undefined, { timeout: 120_000 });
  const rowsText = await page.textContent('#rows');
  const totalsText = await page.textContent('#totals');
  check('swarm page shows contributions', /♥/.test(rowsText) && /pts/.test(rowsText), '');
  check('swarm page shows the banned forger', rowsText.includes('banned (fraud)'));
  check('swarm page shows totals', /batches contributing/.test(totalsText));
  await page.close();
}

// ---- 6. generation engine: batch tallies breed children ---------------------
{
  section('generation engine');
  const page = await ctx.newPage();
  page.on('console', (m) => { if (m.type() === 'error') console.log('gen console.error:', m.text()); });
  await page.goto(`${base}/about.html`);
  const result = await page.evaluate(async () => {
    const wasm = await import('./pkg/flame_wasm.js');
    await wasm.default();
    const { computeFlock } = await import('./js/gens.js');
    const { gen } = await import('./js/net.js');

    const manifest = await (await fetch('genomes/manifest.json')).json();
    const baked = [];
    for (const s of manifest.sheep) {
      const genome = await (await fetch(s.file)).text();
      baked.push({ id: wasm.sheep_id(genome), genome, parents: null, gen: 0, baked: true, name: s.name });
    }
    const g = gen() - 2;
    // batch tallies: baked[0] gets work this generation.
    const batches = [0, 1, 2, 3].map((idx) => ({
      sheepId: baked[0].id, frame: 0, idx, contributor: 'f'.repeat(64), gen: g,
    }));
    const store = {
      allSheep: async () => [],
      allBatches: async () => batches,
      allFraud: async () => [],
    };
    const breedFn = async (a, b, ch) => {
      const childJson = wasm.breed(a, b, ch);
      return { childJson, childId: wasm.sheep_id(childJson) };
    };
    const mutateFn = async (genomeJson, ch, rate) => {
      const childJson = wasm.mutate_genome(genomeJson, ch, rate);
      return { childJson, childId: wasm.sheep_id(childJson) };
    };
    const randomFn = async (seed) => {
      const childJson = wasm.random_genome_json(seed, 3);
      return { childJson, childId: wasm.sheep_id(childJson) };
    };
    const { living } = await computeFlock({
      store, baked, breedFn, mutateFn, randomFn, currentGen: g + 1,
    });
    const records = [...living.values()];
    return {
      size: records.length,
      born: records.filter((r) => r.derived).length,
      mutants: records.filter((r) => r.origin === 'mutant').length,
      immigrants: records.filter((r) => r.origin === 'immigrant').length,
      contribSurvives: living.has(baked[0].id),
    };
  });
  check('gen close with batch work breeds children',
    result.contribSurvives && result.born >= 3 && result.size >= 7,
    `living=${result.size}, born=${result.born}, survives=${result.contribSurvives}`);
  check('mutants + immigrant derived from batch tallies',
    result.mutants === 2 && result.immigrants === 1,
    `mutants=${result.mutants}, immigrants=${result.immigrants}`);
  await page.close();
}

console.log(failures ? `\n${failures} CHECK(S) FAILED` : '\nALL E2E CHECKS PASSED');
await browser.close();
server.close();
process.exit(failures ? 1 : 0);
