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

// Flag needed for WebCrypto Ed25519 on Chromium < 137 (default-on later).
const browser = await chromium.launch({
  args: ['--enable-experimental-web-platform-features'],
});
const ctx = await browser.newContext();
ctx.setDefaultTimeout(120_000);

let failures = 0;
const check = (name, ok, extra = '') => {
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${extra ? ' — ' + extra : ''}`);
  if (!ok) failures++;
};

ctx.on('weberror', (e) => console.log('PAGE ERROR:', e.error().message));

// ---- 1. determinism ---------------------------------------------------------
{
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

  // Vote on the first card in p1; the proof render runs, then p2's tally bumps.
  const firstId = await p1.locator('.card .meta a').first().getAttribute('title');
  await p1.locator('.card button', { hasText: /^vote$/ }).first().click();
  await p1.locator('.card button', { hasText: 'voted ✓' }).first().waitFor({ timeout: 180_000 });
  check('p1 vote completed (proof rendered + signed)', true);

  const tally2 = p2.locator(`.card:has(a[title="${firstId}"]) .tally`);
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
  await p2.locator('#release', { hasText: 'released ✓' }).waitFor({ timeout: 180_000 });
  check('p2 bred and released a child (with proof)', true);

  let received = true;
  await p1.waitForFunction(
    (n) => document.querySelectorAll('.card').length > n, cards1, { timeout: 30_000 },
  ).catch(() => { received = false; });
  const after = await p1.locator('.card').count();
  check('p1 received the released child', received && after > cards1, `${cards1} -> ${after} cards`);

  await p1.close();
  await p2.close();
}

await browser.close();
server.close();
console.log(failures ? `\n${failures} FAILURES` : '\nALL E2E CHECKS PASSED');
process.exit(failures ? 1 : 0);
