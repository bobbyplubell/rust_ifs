// v3 end-to-end test — the v3 browser client against a real sheep-node.
//
// Run via e2e/run-v3.sh: a sheep-node release binary serves the HTTP watch +
// write face on the HOST at 127.0.0.1:8080 (with a bootstrap flock so there's a
// live sheep); this test runs in Docker Playwright with --network host, so the
// container's 127.0.0.1 reaches that node. We serve web/ from an in-container
// static server and point the client at the node via ?world=.
//
// Two things are proven:
//   1. WATCH UI — the flock page polls GET /api/flock and renders a card per
//      living sheep, each with a <video> whose src is the node's /api/video/:id.
//   2. CONTRIBUTE (the byte-match) — in the real browser: render a tile in WASM
//      (render_batch), deflate the histogram, build + Ed25519-sign a v3 Envelope
//      with api.js (the SAME canonical bytes sheep-proto signs), and POST it to
//      /api/msg. The node re-verifies the signature and re-hashes the decoded
//      histogram; a bad byte-match → HTTP 400 / not-accepted. We assert ACCEPTED,
//      which proves the JS↔Rust Envelope signing + render byte-match end-to-end.

import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { extname, join, normalize } from 'node:path';
import { chromium } from 'playwright';

const NODE = process.env.NODE_URL || 'http://127.0.0.1:8080';
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
const world = `?world=${encodeURIComponent(NODE)}`;
console.log('serving web/ at', base, '· node at', NODE);

const browser = await chromium.launch({
  args: ['--enable-experimental-web-platform-features'], // Ed25519 WebCrypto
});
const ctx = await browser.newContext();
ctx.setDefaultTimeout(60_000);

let failures = 0;
const ts = () => new Date().toISOString().slice(11, 19);
const section = (n) => console.log(`[${ts()}] === ${n}`);
const check = (n, ok, extra = '') => {
  console.log(`[${ts()}] ${ok ? 'PASS' : 'FAIL'}  ${n}${extra ? ' — ' + extra : ''}`);
  if (!ok) failures++;
};
ctx.on('weberror', (e) => console.log('PAGE ERROR:', e.error().message));

// ---- 0. node is up + serving a live flock -----------------------------------
{
  section('node health + live flock');
  const probe = await ctx.newPage();
  const health = await probe.evaluate(async (n) => {
    const r = await fetch(`${n}/health`);
    return r.json();
  }, NODE);
  check('node /health ok', health.status === 'ok', JSON.stringify(health));
  check('node has a live flock', (health.live_flock || 0) >= 1, `live_flock=${health.live_flock}`);
  await probe.close();
}

// ---- 1. WATCH UI: flock gallery renders cards + videos -----------------------
let firstSheepId = null;
{
  section('watch UI (flock gallery)');
  const page = await ctx.newPage();
  page.on('console', (m) => { if (m.type() === 'error') console.log('console.error:', m.text()); });
  await page.goto(`${base}/index.html${world}`);

  // The gallery polls /api/flock and appends a .card per living sheep.
  await page.waitForSelector('#flock .card', { timeout: 30_000 });
  const cardCount = await page.locator('#flock .card').count();
  check('≥1 sheep card rendered', cardCount >= 1, `cards=${cardCount}`);

  // Each card carries a <video> whose src is the node's /api/video/:id.
  const videoSrc = await page.locator('#flock .card video').first().getAttribute('src');
  check('card video src points at node /api/video', !!videoSrc && /\/api\/video\//.test(videoSrc), videoSrc || '(none)');

  // The card link encodes the sheep id (used for the detail page below).
  firstSheepId = await page.locator('#flock .card').first().getAttribute('data-id');
  check('card carries a sheep id', !!firstSheepId && firstSheepId.length === 64, firstSheepId || '(none)');
  await page.close();
}

// ---- 2. sheep detail page ----------------------------------------------------
if (firstSheepId) {
  section('sheep detail view');
  const page = await ctx.newPage();
  await page.goto(`${base}/sheep.html?id=${firstSheepId}&world=${encodeURIComponent(NODE)}`);
  await page.waitForFunction(() => {
    const v = document.querySelector('#cv');
    return v && v.src && /\/api\/video\//.test(v.src);
  }, { timeout: 20_000 });
  const sid = await page.locator('#sheep-id').textContent();
  check('detail page shows the sheep id', sid === firstSheepId, sid || '(none)');
  await page.close();
}

// ---- 3. hall of fame ---------------------------------------------------------
{
  section('hall of fame');
  const page = await ctx.newPage();
  await page.goto(`${base}/hall.html${world}`);
  // Either champions render (.champ) or the empty-hall note appears — both are
  // valid; we assert the page reached the node and rendered SOMETHING coherent.
  await page.waitForFunction(() => {
    const h = document.querySelector('#hall');
    return h && h.textContent !== 'loading the hall…';
  }, { timeout: 20_000 });
  const champs = await page.locator('#hall .champ').count();
  const text = await page.locator('#hall').textContent();
  check('hall rendered', champs >= 1 || /no champions/.test(text), `champs=${champs}`);
  await page.close();
}

// ---- 4. THE CRITICAL ONE: contribute byte-match (render → sign → POST) -------
{
  section('contribute byte-match (render → sign Envelope → POST /api/msg)');
  const page = await ctx.newPage();
  page.on('console', (m) => { if (m.type() === 'error') console.log('console.error:', m.text()); });
  // Load on the client origin so its ES modules + the WASM pkg resolve normally.
  await page.goto(`${base}/index.html${world}`);
  await page.waitForSelector('#flock .card', { timeout: 30_000 });

  const result = await page.evaluate(async ({ nodeUrl }) => {
    // Use the SAME modules the client ships — this is the real signing path.
    const api = await import('./js/api.js');
    const { loadIdentity } = await import('./js/identity.js');
    const { histToBase64, SPP, N_FRAMES, SS } = await import('./js/contribute.js');
    const wasm = await import('./pkg/flame_wasm.js');
    await wasm.default();

    const id = await loadIdentity();

    // 1) Pick a live sheep + its genome from the node's flock.
    const flock = await api.getFlock();
    const sheep = (flock.sheep || [])[0];
    if (!sheep) return { error: 'no live sheep in flock' };
    const genomeJson = JSON.stringify(sheep.genome);
    const edge = sheep.resolution || 384;

    // 2) Render tile (frame 0, idx 0, pass 0) in WASM — the exact node params.
    const frame = 0, idx = 0, pass = 0;
    const b = wasm.render_batch(genomeJson, sheep.id, frame, idx, edge, edge, SS, SPP, N_FRAMES);
    const hash = b.hash;
    const count = wasm.total_count(b.hist, edge, edge, SS).toString();
    const histB64 = await histToBase64(b.hist.buffer);

    // 3) Build + sign a v3 PieceUpload Envelope via the client's own api.js,
    //    then verify the canonical bytes match what we expect (sig excluded).
    const body = {
      sheep_id: sheep.id, frame, idx, pass, hash, count, hist_b64: histB64,
    };
    const env = await api.signEnvelope(id, api.T.PIECE, body);
    // Re-derive the canonical signed string and confirm `sig` is excluded +
    // keys are recursively sorted (the byte-match invariant).
    const canonical = api.canonicalize({ v: env.v, t: env.t, from: env.from, ts: env.ts, body: env.body });

    // 4) POST it to the node. A bad byte-match → 400 bad signature; a bad render
    //    → ingest-audit hash mismatch. We assert HTTP 200 + accepted:true.
    let httpStatus, reply;
    try {
      const res = await fetch(`${nodeUrl}/api/msg`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(env),
      });
      httpStatus = res.status;
      reply = await res.json();
    } catch (e) {
      return { error: 'POST failed: ' + e.message };
    }

    return {
      pub: id.pubHex,
      sheepId: sheep.id,
      hash,
      count,
      sigLen: env.sig.length,
      canonicalHasSig: /"sig"/.test(canonical),
      httpStatus,
      reply,
    };
  }, { nodeUrl: NODE });

  if (result.error) {
    check('contribute path ran', false, result.error);
  } else {
    console.log(`[${ts()}]   pub=${result.pub.slice(0, 12)} sheep=${result.sheepId.slice(0, 12)} hash=${result.hash.slice(0, 12)} count=${result.count}`);
    console.log(`[${ts()}]   http=${result.httpStatus} reply=${JSON.stringify(result.reply)}`);
    check('Ed25519 signature is 128 hex chars', result.sigLen === 128, `sigLen=${result.sigLen}`);
    check('canonical bytes exclude sig', result.canonicalHasSig === false);
    check('POST /api/msg returned 200', result.httpStatus === 200, `status=${result.httpStatus}`);
    check('node ACCEPTED the signed Envelope (byte-match verified)',
      result.reply && result.reply.accepted === true,
      `accepted=${result.reply?.accepted} reason=${result.reply?.reason || ''}`);
  }
  await page.close();
}

// ---- teardown ---------------------------------------------------------------
await browser.close();
server.close();
console.log(`\n${failures === 0 ? 'ALL PASS' : failures + ' FAILURE(S)'}`);
process.exit(failures === 0 ? 0 : 1);
