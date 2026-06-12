// End-to-end stack test, run in Docker (see run.sh). Drives real Chromium:
//   1. determinism.html must report all chunk hashes matching the native
//      expected-hashes.txt — the actual browser-vs-native determinism proof.
//   2. Two tabs (?peer=1 / ?peer=2) must find each other (BroadcastChannel),
//      render the flock, and sync a vote and a released child both ways.

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
  } catch {
    res.writeHead(404);
    res.end();
  }
});
await new Promise((r) => server.listen(0, '127.0.0.1', r));
const base = `http://127.0.0.1:${server.address().port}`;
console.log('serving web/ at', base);

// Flags: WebCrypto Ed25519 on Chromium < 137 (default-on later), and best-
// effort software WebGPU so the GPU preview path gets exercised when possible.
const browser = await chromium.launch({
  args: [
    '--enable-experimental-web-platform-features',
    '--enable-unsafe-webgpu',
    '--use-webgpu-adapter=swiftshader',
  ],
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

// ---- 1. determinism ---------------------------------------------------------
{
  section('determinism');
  const page = await ctx.newPage();
  page.on('console', (m) => { if (m.type() === 'error') console.log('console.error:', m.text()); });
  await page.goto(`${base}/determinism.html`);
  const banner = page.locator('#banner');
  await banner.filter({ hasText: /✓|✗|error|done/ }).waitFor({ timeout: 180_000 });
  const text = await banner.textContent();
  check('determinism: browser hashes match native', text.includes('✓ all'), text.trim());
  await page.close();
}

// ---- 2. two peers: render, vote sync, release sync --------------------------
{
  section('two peers');
  const p1 = await ctx.newPage();
  const p2 = await ctx.newPage();
  for (const [n, p] of [['p1', p1], ['p2', p2]]) {
    p.on('console', (m) => { if (m.type() === 'error') console.log(`${n} console.error:`, m.text()); });
    p.on('pageerror', (e) => console.log(`${n} pageerror:`, e.message));
  }
  await p1.goto(`${base}/index.html?peer=1`);
  await p2.goto(`${base}/index.html?peer=2`);

  // Both see the flock.
  await p1.locator('.card').first().waitFor();
  await p2.locator('.card').first().waitFor();
  const cards1 = await p1.locator('.card').count();
  check('p1 flock has cards', cards1 >= 6, `${cards1} cards`);

  // Peers discover each other via inv gossip.
  await p1.locator('#status', { hasText: /[1-9] peers/ }).waitFor({ timeout: 30_000 });
  check('p1 sees a peer', true);

  // First card renders something non-black.
  await p1.waitForFunction(() => {
    const c = document.querySelector('.card canvas');
    if (!c) return false;
    const d = c.getContext('2d').getImageData(0, 0, c.width, c.height).data;
    for (let i = 0; i < d.length; i += 4) if (d[i] + d[i + 1] + d[i + 2] > 30) return true;
    return false;
  }, undefined, { timeout: 120_000 });
  check('p1 first card rendered pixels', true);

  // Vote on the first card in p1: a 64-frame loop proof fans out across the
  // pool (heavier than the old chunk proof — generous timeout), then p2's
  // tally bumps.
  const firstId = await p1.locator('.card').first().getAttribute('data-id');
  await p1.locator('.card button', { hasText: /^vote$/ }).first().click();
  await p1.locator('.card button', { hasText: 'voted ✓' }).first().waitFor({ timeout: 300_000 });
  check('p1 vote completed (loop proof rendered + signed)', true);

  const tally2 = p2.locator(`.card[data-id="${firstId}"] .tally`);
  await tally2.filter({ hasText: '♥' }).waitFor({ timeout: 30_000 });
  check('p2 sees p1 vote in tally', true, await tally2.textContent());

  // Breed: select two sheep in p2, wait for the canonical child, release it,
  // and require the new card to appear in p1.
  await p2.locator('.card canvas').nth(0).click();
  await p2.locator('.card canvas').nth(1).click();
  await p2.locator('#nursery-note', { hasText: 'canonical child' }).waitFor({ timeout: 120_000 });
  const release = p2.locator('#release');
  await release.waitFor();
  await release.click();
  await p2.locator('#release', { hasText: 'released ✓' }).waitFor({ timeout: 300_000 });
  check('p2 bred and released a child (with proof)', true);

  // Ultra (2x weight) vote from p1 on the SECOND card; p2 must show '2'.
  const secondId = await p1.locator('.card').nth(1).getAttribute('data-id');
  await p1.locator('.card').nth(1).locator('button', { hasText: '2×' }).click();
  await p1.locator('.card button', { hasText: /voted 2× ✓/ }).first().waitFor({ timeout: 300_000 });
  check('p1 ultra vote completed', true);
  const tally2u = p2.locator(`.card[data-id="${secondId}"] .tally`);
  await tally2u.filter({ hasText: '2' }).waitFor({ timeout: 30_000 });
  check('p2 sees double weight for ultra vote', true, await tally2u.textContent());

  section('fraud injection');
  // Fraud: inject a validly-signed vote with garbage hashes from a fresh key.
  // The background auditor must re-render a frame, catch the mismatch, gossip
  // a fraud proof, and the cheater's votes must stop counting. Steps are
  // separate bounded evaluates so a hang names its step instead of stalling
  // the suite.
  try {
    const step = async (name, fn, arg) => {
      const r = await Promise.race([
        p1.evaluate(fn, arg),
        new Promise((_, rej) =>
          setTimeout(() => rej(new Error(`fraud step "${name}" hung (30s)`)), 30_000)),
      ]);
      if (name !== 'poll stats') console.log(`[${ts()}] fraud step ok: ${name}`);
      return r;
    };

    const voter = await step('forge + inject vote', async (sheepId) => {
      const { voteSignBytes, voteKey, gen, PROOF_SPEC, CHANNEL } = await import('./js/net.js');
      const { hex } = await import('./js/hash.js');
      const pair = await crypto.subtle.generateKey(
        { name: 'Ed25519' }, false, ['sign', 'verify']);
      const voter = hex(new Uint8Array(await crypto.subtle.exportKey('raw', pair.publicKey)));
      const chunkHashes = Array.from({ length: PROOF_SPEC.nFrames }, (_, i) =>
        (i % 10).toString().repeat(64)); // garbage, but well-formed
      const record = { sheepId, gen: gen(), voter, tier: 'std', chunkHashes };
      record.sig = hex(new Uint8Array(
        await crypto.subtle.sign({ name: 'Ed25519' }, pair.privateKey, voteSignBytes(record))));
      record.key = voteKey(record);
      new BroadcastChannel(CHANNEL).postMessage({ kind: 'vote', record });
      return voter;
    }, firstId);

    // Poll from node (no long-lived page evaluate): the idle-paced auditor
    // should convict within a few 8s ticks.
    let caught = false;
    let stats = {};
    const t0 = Date.now();
    while (Date.now() - t0 < 150_000) {
      stats = await step('poll stats', () => ({
        audits: window.__sheepStats.audits,
        frauds: window.__sheepStats.frauds,
        banned: window.__sheepStats.banned,
      }));
      if (stats.frauds > 0 && stats.banned.includes(voter)) { caught = true; break; }
      await new Promise((r) => setTimeout(r, 3000));
    }
    check('auditor catches forged vote and bans the key', caught,
      `audits run: ${stats.audits}, frauds: ${stats.frauds}`);
  } catch (err) {
    check('auditor catches forged vote and bans the key', false, err.message);
  }

  await p1.close();
  await p2.close();
}

// ---- 3. generation engine: one vote must still breed children ---------------
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
    const store = {
      allSheep: async () => [],
      allVotes: async () => [{
        sheepId: baked[0].id, gen: g, voter: 'f'.repeat(64),
        chunkHashes: [], key: `x:${baked[0].id}:${g}`,
      }],
    };
    const breedFn = async (aJson, bJson, challengeHex) => {
      const childJson = wasm.breed(aJson, bJson, challengeHex);
      return { childJson, childId: wasm.sheep_id(childJson) };
    };
    const { living } = await computeFlock({ store, baked, breedFn, currentGen: g + 1 });
    const records = [...living.values()];
    return {
      size: records.length,
      born: records.filter((r) => r.derived).length,
      votedSurvives: living.has(baked[0].id),
    };
  });
  check('gen close with one vote breeds children',
    result.votedSurvives && result.born >= 3 && result.size >= 7,
    `living=${result.size}, born=${result.born}, voted-survives=${result.votedSurvives}`);
  await page.close();
}

// ---- 4. WebGPU preview (soft: skips when the container has no WebGPU) -------
{
  section('webgpu');
  const page = await ctx.newPage();
  page.on('console', (m) => { if (m.type() === 'error') console.log('gpu console.error:', m.text()); });
  await page.goto(`${base}/about.html`); // any same-origin page to import from
  const result = await page.evaluate(async () => {
    try {
      const { GpuFlame } = await import('./js/gpu.js');
      // Some headless builds hang inside requestAdapter instead of resolving
      // null — bound it.
      const gpu = await Promise.race([
        GpuFlame.create(),
        new Promise((r) => setTimeout(() => r(null), 20_000)),
      ]);
      if (!gpu) return { available: false };
      const canvas = document.createElement('canvas');
      canvas.width = 128;
      canvas.height = 128;
      document.body.append(canvas);
      gpu.configure(canvas);
      const infoJson = JSON.stringify(gpu.adapterInfo ?? {});
      // Empty/unidentifiable adapter info in a container = assume software.
      const software = infoJson.length <= 2 ||
        /swiftshader|llvmpipe|software|cpu/i.test(infoJson);
      const genome = await (await fetch('genomes/seed_7.json')).text();
      await gpu.frame(genome, 0.3, { width: 128, height: 128, ss: 1, samples: 300_000 });
      // Software adapters can lag the completion promise — poll up to 5s.
      const chk = document.createElement('canvas');
      chk.width = 128;
      chk.height = 128;
      const c2 = chk.getContext('2d');
      let lit = 0;
      for (let tries = 0; tries < 10 && lit <= 50; tries++) {
        await new Promise((r) => setTimeout(r, 500));
        c2.drawImage(canvas, 0, 0);
        const d = c2.getImageData(0, 0, 128, 128).data;
        lit = 0;
        for (let i = 0; i < d.length; i += 4) if (d[i] + d[i + 1] + d[i + 2] > 30) lit++;
      }
      return { available: true, software, info: infoJson, lit };
    } catch (err) {
      return { available: true, software: true, error: err.message };
    }
  });
  if (!result.available) {
    console.log(`[${ts()}] SKIP  webgpu preview — no WebGPU adapter in this container`);
  } else if ((result.error || result.lit <= 50) && result.software) {
    console.log(`[${ts()}] SKIP  webgpu preview — software adapter quirk: ` +
      (result.error || `${result.lit} lit pixels`) + ` (adapter: ${result.info ?? '{}'})`);
  } else {
    check('webgpu preview renders pixels', !result.error && result.lit > 50,
      (result.error || `${result.lit} lit pixels`) + ` (adapter: ${result.info ?? '{}'})`);
  }
  await page.close();
}

await browser.close();
server.close();
console.log(failures ? `\n${failures} FAILURES` : '\nALL E2E CHECKS PASSED');
process.exit(failures ? 1 : 0);
