// One swarm peer = one headless browser in its own container. It loads the
// site pointed at the relay (?relay=), drives contributions, and prints its
// metrics dump to stdout for the orchestrator to collect.
import { chromium } from 'playwright';

const WEB = process.env.WEB_URL || 'https://swarm';
const RELAY = process.env.RELAY_MADDR;
const PEER = process.env.HOSTNAME || ('c' + Math.floor(Math.random() * 1e6));
const MINUTES = +(process.env.MINUTES || 3);
const CONTRIB_MS = +(process.env.CONTRIB_MS || 4000);
const log = (...a) => console.log(`[${PEER}]`, ...a);

const LP_DEBUG = process.env.LP_DEBUG; // e.g. 'libp2p:circuit-relay*'
const browser = await chromium.launch({
  args: ['--ignore-certificate-errors', '--enable-experimental-web-platform-features',
    '--no-sandbox', '--disable-gpu'],
});
const ctx = await browser.newContext({ ignoreHTTPSErrors: true });
if (LP_DEBUG) await ctx.addInitScript((d) => { try { localStorage.debug = d; } catch {} }, LP_DEBUG);
const page = await ctx.newPage();
page.on('pageerror', (e) => log('ERR', e.message.slice(0, 140)));
page.on('console', (m) => {
  if (m.type() === 'error') log('console.error:', m.text().slice(0, 140));
  else if (LP_DEBUG && /relay|reserv|webrtc|circuit/i.test(m.text())) log('DBG', m.text().slice(0, 200));
});

const url = `${WEB}/index.html?peer=${PEER}&stress=1&workers=1&relay=${encodeURIComponent(RELAY)}`;
await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 60000 });
await page.waitForFunction(() => !!window.__sheepAct, { timeout: 60000 });
log('up; relay =', RELAY);

// --- libp2p diagnostics: track discovery + connection events ---
await page.evaluate(() => {
  const n = window.__libp2p;
  window.__lp = { discovered: 0, connect: 0, disconnect: 0, dialErr: [], lastAddrs: [] };
  if (!n) return;
  n.addEventListener('peer:discovery', () => { window.__lp.discovered++; });
  n.addEventListener('peer:connect', () => { window.__lp.connect++; });
  n.addEventListener('peer:disconnect', () => { window.__lp.disconnect++; });
});
const lpDiag = async () => {
  try {
    return await page.evaluate(() => {
      const n = window.__libp2p; if (!n) return null;
      const conns = n.getConnections();
      return {
        ...window.__lp,
        conns: conns.length,
        // 'rtc' = direct browser-to-browser WebRTC, 'ws' = relay link.
        remotes: conns.map((c) => (c.remoteAddr.toString().includes('webrtc') ? 'rtc' : 'ws')),
      };
    });
  } catch (e) { return { err: e.message }; }
};

const dump = async () => {
  try { log('DUMP', JSON.stringify(await page.evaluate(() => window.__sheepDump()))); }
  catch (e) { log('dump err', e.message); }
};

const end = Date.now() + MINUTES * 60_000;
let lastDump = 0;
while (Date.now() < end) {
  await page.evaluate(() => window.__sheepAct.contributeRandom()).catch(() => {});
  if (Date.now() - lastDump > 30_000) { await dump(); log('LP', JSON.stringify(await lpDiag())); lastDump = Date.now(); }
  await new Promise((r) => setTimeout(r, CONTRIB_MS));
}
// Settle: quiesce the swarm (stop BOTH the harness loop and the app's own
// background contribute loop) and let anti-entropy converge before the FINAL
// dump (the convergence check reads the last dump per peer).
log('settling');
await page.evaluate(() => window.__sheepAct.pauseContribute(true)).catch(() => {});
await new Promise((r) => setTimeout(r, +(process.env.SETTLE_MS || 60_000)));
log('LP', JSON.stringify(await lpDiag()));
await dump();
log('done');
await browser.close();
process.exit(0);
