// Swarm stress harness: N simulated peers in ONE Chromium profile (pages in
// one context share the BroadcastChannel bus; ?peer=i namespaces identity +
// store), with a driver that schedules votes/breeds under a render semaphore
// and samples metrics each tick. Run via stress/run.sh (Docker, no host Node).
//
// Env knobs:
//   PEERS=50         simulated peers (pages)
//   MINUTES=10       test duration
//   WORKERS=1        worker-pool size per peer
//   RENDER_SLOTS=4   max concurrent proof renders (CPU governor)
//   VOTE_RATE=2      target votes/minute swarm-wide
//   BREED_RATE=0.3   target breed+releases/minute swarm-wide
//   SAMPLE=20        peers sampled per metrics tick
//   OUT=/repo/stress/out.jsonl   metrics stream (JSONL)

import { createServer } from 'node:http';
import { readFile, appendFile, writeFile } from 'node:fs/promises';
import { exec } from 'node:child_process';
import { extname, join, normalize } from 'node:path';
import { chromium } from 'playwright';

const PEERS = +(process.env.PEERS ?? 50);
const MINUTES = +(process.env.MINUTES ?? 10);
const WORKERS = +(process.env.WORKERS ?? 1);
const RENDER_SLOTS = +(process.env.RENDER_SLOTS ?? 4);
const VOTE_RATE = +(process.env.VOTE_RATE ?? 2);
const BREED_RATE = +(process.env.BREED_RATE ?? 0.3);
const SAMPLE = Math.min(+(process.env.SAMPLE ?? 20), PEERS);
const OUT = process.env.OUT ?? new URL('./out.jsonl', import.meta.url).pathname;

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

const ts = () => new Date().toISOString().slice(11, 19);
const log = (...a) => console.log(`[${ts()}]`, ...a);
await writeFile(OUT, '');
const emit = (obj) => appendFile(OUT, JSON.stringify(obj) + '\n');

log(`swarm: ${PEERS} peers, ${MINUTES}min, ${WORKERS} worker(s)/peer, ` +
  `${RENDER_SLOTS} render slots, ${VOTE_RATE} votes/min, ${BREED_RATE} breeds/min`);

const browser = await chromium.launch({
  args: [
    '--enable-experimental-web-platform-features',
    // Pack same-origin pages into fewer renderer processes.
    '--renderer-process-limit=32',
    '--disable-gpu',
  ],
});
const ctx = await browser.newContext();
ctx.setDefaultTimeout(120_000);

let crashes = 0;
const pages = [];
for (let i = 0; i < PEERS; i++) {
  const page = await ctx.newPage();
  page.on('crash', () => { crashes++; log(`peer s${i} CRASHED (total ${crashes})`); });
  page.on('pageerror', (e) => log(`peer s${i} pageerror: ${e.message.slice(0, 120)}`));
  await page.goto(`${base}/index.html?peer=s${i}&workers=${WORKERS}&stress=1`);
  pages.push(page);
  if (i % 25 === 24) log(`spawned ${i + 1}/${PEERS}`);
  await new Promise((r) => setTimeout(r, 80)); // stagger
}
log(`all ${PEERS} peers up; waiting for flocks`);
await pages[0].locator('.card').first().waitFor();

// ---- behavior driver --------------------------------------------------------

let renderSlots = RENDER_SLOTS;
let votesDone = 0;
let breedsDone = 0;
let actionsFailed = 0;

function act(kind) {
  if (renderSlots <= 0) return;
  renderSlots--;
  const page = pages[Math.floor(Math.random() * pages.length)];
  const t0 = Date.now();
  page.evaluate(
    (k) => (k === 'vote' ? window.__sheepAct.voteRandom() : window.__sheepAct.breedRandom()),
    kind,
  )
    .then((id) => {
      if (id) {
        kind === 'vote' ? votesDone++ : breedsDone++;
        emit({ t: Date.now(), kind, ms: Date.now() - t0 });
      }
    })
    .catch(() => { actionsFailed++; })
    .finally(() => { renderSlots++; });
}

const voteTimer = setInterval(() => act('vote'), 60_000 / VOTE_RATE);
const breedTimer = setInterval(() => act('breed'), 60_000 / BREED_RATE);

// ---- metrics ---------------------------------------------------------------

const rss = () => new Promise((resolve) => {
  exec("ps -eo rss --no-headers | awk '{s+=$1} END {print s}'", (e, out) =>
    resolve(e ? -1 : Math.round(parseInt(out, 10) / 1024)));
});

async function sampleMetrics() {
  const picks = [...pages].sort(() => Math.random() - 0.5).slice(0, SAMPLE);
  const dumps = (await Promise.all(picks.map((p) =>
    Promise.race([
      p.evaluate(() => window.__sheepDump?.()),
      new Promise((r) => setTimeout(() => r(null), 10_000)),
    ]).catch(() => null),
  ))).filter(Boolean);
  if (!dumps.length) return log('metrics: no responsive peers in sample!');

  const nums = (f) => dumps.map(f).sort((a, b) => a - b);
  const med = (a) => a[Math.floor(a.length / 2)];
  const votes = nums((d) => d.votes);
  const fingerprints = new Set(dumps.map((d) => d.tallyFingerprint));
  const sentBytes = nums((d) => d.net.sentBytes);
  const row = {
    t: Date.now(), kind: 'sample', peers: PEERS, sampled: dumps.length,
    votesMin: votes[0], votesMed: med(votes), votesMax: votes.at(-1),
    distinctTallyViews: fingerprints.size,
    sumsMed: med(nums((d) => d.sums)),
    auditsMed: med(nums((d) => d.audits)),
    fraudTotal: dumps.reduce((a, d) => a + d.fraud, 0),
    sentBytesMed: med(sentBytes),
    invSentMed: med(nums((d) => d.net.sent?.inv ?? 0)),
    rssMB: await rss(),
    crashes, votesDone, breedsDone, actionsFailed,
  };
  emit(row);
  log(`votes ${row.votesMin}/${row.votesMed}/${row.votesMax} ` +
    `views=${row.distinctTallyViews} sums=${row.sumsMed} audits=${row.auditsMed} ` +
    `sentB=${(row.sentBytesMed / 1024).toFixed(0)}k rss=${row.rssMB}MB ` +
    `done v${votesDone}/b${breedsDone} fail=${actionsFailed} crash=${crashes}`);
}

const metricsTimer = setInterval(() => sampleMetrics().catch(console.error), 30_000);

// ---- run + final convergence audit -------------------------------------------

await new Promise((r) => setTimeout(r, MINUTES * 60_000));
clearInterval(voteTimer);
clearInterval(breedTimer);
clearInterval(metricsTimer);
log('duration over; settling 60s for sync convergence');
await new Promise((r) => setTimeout(r, 60_000));

const finals = (await Promise.all(pages.map((p) =>
  Promise.race([
    p.evaluate(() => window.__sheepDump?.()),
    new Promise((r) => setTimeout(() => r(null), 15_000)),
  ]).catch(() => null),
))).filter(Boolean);
const voteCounts = new Set(finals.map((d) => d.votes));
const views = new Set(finals.map((d) => d.tallyFingerprint));
const verdict = {
  t: Date.now(), kind: 'final', peers: PEERS, responsive: finals.length,
  crashes, votesDone, breedsDone, actionsFailed,
  distinctVoteCounts: voteCounts.size,
  distinctTallyViews: views.size,
  converged: voteCounts.size === 1 && views.size === 1,
};
emit(verdict);
log('FINAL:', JSON.stringify(verdict));
log(verdict.converged
  ? 'CONVERGED: every responsive peer holds identical votes and tallies'
  : 'NOT CONVERGED — inspect out.jsonl (vote counts ' +
    [...voteCounts].slice(0, 5).join(',') + ')');

await browser.close();
server.close();
process.exit(verdict.responsive < PEERS * 0.9 || crashes > PEERS * 0.05 ? 1 : 0);
