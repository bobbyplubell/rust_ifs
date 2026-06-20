import { firefox } from 'playwright';
(async () => {
  const b = await firefox.launch();
  const p = await b.newPage();
  const events = [];
  // Capture each /api/msg request's body (sheep ids + types) paired with its
  // response status + body, so we see EXACTLY what gets 422'd and why.
  p.on('response', async (res) => {
    const req = res.request();
    if (!req.url().includes('/api/msg')) return;
    if (req.method() !== 'POST') return;
    let reqSheep = '?', types = '?';
    try {
      const body = JSON.parse(req.postData() || 'null');
      const arr = Array.isArray(body) ? body : (body?.results ? [] : [body]);
      types = arr.map((e) => (e.t || '').split('/').pop()).join(',');
      reqSheep = [...new Set(arr.map((e) => (e.body?.sheep_id || '').slice(0, 12)))].join(',');
    } catch {}
    let respReason = '';
    try {
      const j = await res.json();
      respReason = JSON.stringify(j).slice(0, 160);
    } catch {}
    events.push({ status: res.status(), types, reqSheep, respReason });
  });
  await p.goto('https://proof-of-sheep.com', { waitUntil: 'load', timeout: 30000 }).catch((e) => console.log('goto', e.message));
  await p.waitForTimeout(9000);
  // Capture the flock the browser sees (its cached sheep ids).
  const flockIds = await p.evaluate(async () => {
    try {
      const r = await fetch('/api/flock').then((x) => x.json());
      return (r.sheep || []).map((s) => s.id.slice(0, 12));
    } catch (e) { return ['err:' + e.message]; }
  }).catch(() => ['eval-failed']);
  console.log('browser flock view:', JSON.stringify(flockIds));
  const btn = await p.$('#contribute');
  if (btn) await btn.click();
  await p.waitForTimeout(75000);

  // Summarize by (status, sheep).
  const byKey = {};
  for (const e of events) {
    const k = `${e.status} types=${e.types} sheep=${e.reqSheep}`;
    byKey[k] = byKey[k] || { n: 0, reason: e.respReason };
    byKey[k].n++;
  }
  console.log('=== /api/msg by (status, types, sheep) ===');
  for (const [k, v] of Object.entries(byKey)) console.log(`${v.n}x  ${k}  reason=${v.reason}`);
  await b.close();
})();
