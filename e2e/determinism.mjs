// v3 browser-vs-native determinism check (CI). Serves web/ over HTTP, loads
// web/determinism.html in real Chromium, and asserts the page's banner resolves
// to PASS — i.e. every browser WASM chunk hash is byte-identical to the native
// goldens in web/genomes/expected-hashes.txt. This is the trust boundary of v3
// (an auditor re-renders and compares hashes), so it's the one browser check
// worth guarding after the v2 e2e/connectivity suites were retired.
//
// No Node on the host: run in Docker via e2e/run-determinism.sh (Playwright
// image ships the browsers), exactly like the old e2e harness.
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

// Ed25519 in WebCrypto needs the experimental flag before Chromium 137.
const browser = await chromium.launch({ args: ['--enable-experimental-web-platform-features'] });
const ctx = await browser.newContext();
ctx.setDefaultTimeout(180_000);
const page = await ctx.newPage();
page.on('console', (m) => console.log('[browser]', m.text()));
page.on('pageerror', (e) => console.log('[pageerror]', e.message));

let code = 1;
try {
  await page.goto(`${base}/determinism.html`);
  // The page renders every chunk on the main thread, then sets #banner's class
  // to 'pass' or 'fail'. Wait for it to resolve (generous: no workers).
  await page.waitForFunction(() => {
    const b = document.getElementById('banner');
    return b && (b.classList.contains('pass') || b.classList.contains('fail'));
  }, { timeout: 180_000 });
  const cls = (await page.getAttribute('#banner', 'class')) || '';
  const txt = (await page.textContent('#banner')) || '';
  console.log('banner:', JSON.stringify(cls), '—', txt.trim());
  if (cls.includes('pass')) { console.log('PASS: browser render == native goldens'); code = 0; }
  else { console.error('FAIL: browser determinism mismatch'); code = 1; }
} catch (err) {
  console.error('error driving determinism.html:', err && err.message ? err.message : err);
  code = 1;
} finally {
  await browser.close();
  server.close();
  process.exit(code);
}
