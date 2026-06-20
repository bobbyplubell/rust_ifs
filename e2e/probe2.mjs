import { firefox } from 'playwright';
// Two CLEAN browser contexts (fresh storage), both contributing on the DEFAULT
// gateway (relay1), to measure whether two browsers cross-audit each other and
// drive "accepted" up fast — the user's expectation.
(async () => {
  const b = await firefox.launch();
  const mk = async (tag) => {
    const ctx = await b.newContext(); // isolated storage (no stale localStorage)
    const p = await ctx.newPage();
    let r422 = 0, rOk = 0;
    p.on('response', (res) => {
      if (!res.request().url().includes('/api/msg')) return;
      if (res.status() === 422) r422++; else if (res.ok()) rOk++;
    });
    await p.goto('https://proof-of-sheep.com', { waitUntil: 'load', timeout: 30000 }).catch(() => {});
    await p.waitForTimeout(8000);
    const btn = await p.$('#contribute');
    if (btn) await btn.click();
    return { p, tag, get r422() { return r422; }, get rOk() { return rOk; } };
  };
  const a = await mk('A');
  const c = await mk('B');
  const read = async (x) => {
    const rendered = await x.p.$eval('#stat-rendered', (e) => e.textContent).catch(() => '?');
    const accepted = await x.p.$eval('#stat-accepted', (e) => e.textContent).catch(() => '?');
    return `${x.tag}: rendered=${rendered} accepted=${accepted} msgOk=${x.rOk} msg422=${x.r422}`;
  };
  for (let i = 0; i < 8; i++) {
    await a.p.waitForTimeout(15000);
    console.log(`t+${(i + 1) * 15 + 8}s  ${await read(a)}  |  ${await read(c)}`);
  }
  await b.close();
})();
