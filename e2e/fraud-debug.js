// Focused reproduction of the post-fraud-injection main-thread freeze.
// One page, forged vote, 1s heartbeat, ALL console forwarded with timestamps.

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
    const body = await readFile(file);
    res.writeHead(200, { 'content-type': MIME[extname(file)] || 'application/octet-stream' });
    res.end(body);
  } catch { res.writeHead(404); res.end(); }
});
await new Promise((r) => server.listen(0, '127.0.0.1', r));
const base = `http://127.0.0.1:${server.address().port}`;

const browser = await chromium.launch({
  args: ['--enable-experimental-web-platform-features', '--enable-logging=stderr', '--v=0'],
});
const ctx = await browser.newContext();
const page = await ctx.newPage();
const ts = () => new Date().toISOString().slice(11, 23);
page.on('console', (m) => console.log(`[${ts()}] [${m.type()}]`, m.text()));
page.on('pageerror', (e) => console.log(`[${ts()}] PAGEERROR`, e.message));
page.on('crash', () => console.log(`[${ts()}] *** PAGE CRASHED (renderer process died) ***`));
page.on('worker', (w) => {
  console.log(`[${ts()}] worker spawned: ${w.url().split('/').pop()}`);
  w.on('close', () => console.log(`[${ts()}] *** WORKER CLOSED ***`));
});

// Bisection toggles: NOAUDIT=1 disables the auditor, NOINJECT=1 skips the
// forged vote. RSS of the renderer is sampled every 5s.
import('node:child_process').then(({ exec }) => {
  setInterval(() => {
    exec("ps -eo rss,comm --no-headers | sort -rn | head -3", (e, out) => {
      if (!e) console.log(`[${ts()}] RSS top: ${out.trim().replace(/\n/g, ' | ')}`);
    });
  }, 5000);
});

await page.goto(`${base}/index.html?peer=1${process.env.NOAUDIT ? '&noaudit=1' : ''}`);
await page.locator('.card').first().waitFor({ timeout: 120_000 });
console.log(`[${ts()}] page up, starting heartbeat + injecting forged vote`);

await page.evaluate(() => {
  let n = 0;
  setInterval(() => console.log('hb', n++), 1000);
});

if (!process.env.NOINJECT) await page.evaluate(async (unused) => {
  const { voteSignBytes, voteKey, gen, PROOF_SPEC, CHANNEL } = await import('./js/net.js');
  const { hex } = await import('./js/hash.js');
  const sheepId = document.querySelector('.card').dataset.id;
  const pair = await crypto.subtle.generateKey({ name: 'Ed25519' }, false, ['sign', 'verify']);
  const voter = hex(new Uint8Array(await crypto.subtle.exportKey('raw', pair.publicKey)));
  const chunkHashes = Array.from({ length: PROOF_SPEC.nFrames }, (_, i) =>
    (i % 10).toString().repeat(64));
  const record = { sheepId, gen: gen(), voter, tier: 'std', sumHash: 'ab'.repeat(32), chunkHashes };
  record.sig = hex(new Uint8Array(
    await crypto.subtle.sign({ name: 'Ed25519' }, pair.privateKey, voteSignBytes(record))));
  record.key = voteKey(record);
  new BroadcastChannel(CHANNEL).postMessage({ kind: 'vote', record });
  console.log('forged vote injected for', sheepId.slice(0, 12), 'by', voter.slice(0, 12));
}, 0);

// Observe for 100s, probing liveness from outside every 10s.
for (let i = 0; i < 10; i++) {
  await new Promise((r) => setTimeout(r, 10_000));
  const alive = await Promise.race([
    page.evaluate(() => 'alive').catch((e) => 'evaluate-error: ' + e.message),
    new Promise((r) => setTimeout(() => r('EVALUATE HUNG'), 5_000)),
  ]);
  console.log(`[${ts()}] probe ${i}: ${alive}`);
}
console.log(`[${ts()}] observation window over`);
await browser.close();
server.close();
process.exit(0);
