import { firefox } from 'playwright';
// One browser; intercept /api/assign responses and /api/msg replies to see
// whether the work frontier ADVANCES (new tiles) or REPEATS, and how the
// submitter-credit (confirmed_tiles) + per-sheep coverage move.
(async () => {
  const b = await firefox.launch();
  const p = await b.newPage();
  const assigns = [];
  p.on('response', async (res) => {
    const u = res.request().url();
    if (u.includes('/api/assign')) {
      try {
        const j = await res.json();
        const blocks = (j.blocks || []).map((x) => `${x.sheep_id.slice(0,6)}:${x.frame}.${x.idx}.${x.pass}`);
        const audits = (j.audits || []).map((x) => `${x.sheep_id.slice(0,6)}:${x.frame}.${x.idx}.${x.pass}`);
        assigns.push({ t: Date.now(), blocks, audits });
      } catch {}
    }
  });
  await p.goto('https://proof-of-sheep.com', { waitUntil: 'load', timeout: 30000 }).catch(() => {});
  await p.waitForTimeout(8000);
  const btn = await p.$('#contribute');
  if (btn) await btn.click();
  await p.waitForTimeout(60000);

  // How many DISTINCT block tiles were ever assigned vs total assignments — if the
  // frontier advances, distinct ≈ total; if it repeats, distinct ≪ total.
  const allBlocks = assigns.flatMap((a) => a.blocks);
  const distinctBlocks = new Set(allBlocks);
  const allAudits = assigns.flatMap((a) => a.audits);
  const distinctAudits = new Set(allAudits);
  console.log(`assign calls: ${assigns.length}`);
  console.log(`BLOCK tiles assigned: total=${allBlocks.length} distinct=${distinctBlocks.size}`);
  console.log(`AUDIT tiles assigned: total=${allAudits.length} distinct=${distinctAudits.size}`);
  // Sample the assigned blocks at start, middle, end.
  const pick = (i) => assigns[i] ? `blocks=[${assigns[i].blocks.slice(0,6).join(' ')}] audits=[${assigns[i].audits.slice(0,4).join(' ')}]` : 'none';
  console.log('first :', pick(0));
  console.log('middle:', pick(Math.floor(assigns.length / 2)));
  console.log('last  :', pick(assigns.length - 1));
  const rendered = await p.$eval('#stat-rendered', (e) => e.textContent).catch(() => '?');
  const accepted = await p.$eval('#stat-accepted', (e) => e.textContent).catch(() => '?');
  console.log(`final rendered=${rendered} accepted=${accepted}`);
  await b.close();
})();
