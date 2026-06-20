import { firefox } from 'playwright';
// STRESS / STABILITY TEST: two CLEAN browsers both contributing, run until each
// reaches TARGET rendered AND TARGET accepted, or a hard timeout. Reports the
// rendered/accepted/422 trajectory so we can SEE accepted keep pace with rendered
// (not freeze). Exit 0 if both hit the target, 1 otherwise.
const TARGET = Number(process.env.TARGET || 1000);
const TIMEOUT_MS = Number(process.env.TIMEOUT_MS || 12 * 60 * 1000);

(async () => {
  const b = await firefox.launch();
  const mk = async (tag) => {
    const ctx = await b.newContext(); // fresh storage — no stale gateway/identity
    const p = await ctx.newPage();
    let r422 = 0;
    p.on('response', (res) => {
      if (res.request().url().includes('/api/msg') && res.status() === 422) r422++;
    });
    await p.goto('https://proof-of-sheep.com', { waitUntil: 'load', timeout: 30000 }).catch(() => {});
    await p.waitForTimeout(8000);
    const btn = await p.$('#contribute');
    if (btn) await btn.click();
    return { p, tag, get r422() { return r422; } };
  };
  const A = await mk('A');
  const B = await mk('B');
  const num = async (x, sel) => {
    const t = await x.p.$eval(sel, (e) => e.textContent).catch(() => '0');
    const n = parseInt(String(t).replace(/[^0-9]/g, ''), 10);
    return Number.isFinite(n) ? n : 0;
  };
  const t0 = Date.now();
  let pass = false;
  while (Date.now() - t0 < TIMEOUT_MS) {
    await A.p.waitForTimeout(15000);
    const ar = await num(A, '#stat-rendered'), aa = await num(A, '#stat-accepted');
    const br = await num(B, '#stat-rendered'), ba = await num(B, '#stat-accepted');
    const secs = Math.round((Date.now() - t0) / 1000);
    console.log(`t+${secs}s  A: rendered=${ar} accepted=${aa} 422=${A.r422}  |  B: rendered=${br} accepted=${ba} 422=${B.r422}`);
    if (ar >= TARGET && aa >= TARGET && br >= TARGET && ba >= TARGET) { pass = true; break; }
  }
  console.log(pass ? `PASS: both browsers reached ${TARGET}+ rendered AND accepted` : `FAIL: target ${TARGET} not reached within ${TIMEOUT_MS / 1000}s`);
  await b.close();
  process.exit(pass ? 0 : 1);
})();
