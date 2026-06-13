// gens.js — the generation engine: a pure function from the fact store to the
// living flock. Nothing here is gossiped or signed; every peer recomputes the
// same lineage from the same facts (children are derived via the deterministic
// wasm breeder), so agreement needs no consensus.
//
// Population pressure (the point of generations): survivors are a fixed top-K
// by tally, automatic births are bounded by K, submissions cost a render proof
// and are capped per (author, generation) — so the flock cannot blow up with
// peer count. Quiet generations (no votes) carry the flock forward unchanged.

import { gen, SURVIVORS_K, AUTHOR_GEN_CAP, MUTANTS_PER_GEN, IMMIGRANTS_PER_GEN } from './net.js';
import { sha256Hex, utf8 } from './hash.js';

// Canonical child challenge for a pair in generation g (ids sorted, so a pair
// has one child regardless of who computes it). Also used by the nursery
// preview in app.js — the preview IS the canonical child.
export const breedChallenge = (g, idA, idB) => {
  const [x, y] = [idA, idB].sort();
  return sha256Hex(utf8(`breed|${g}|${x}|${y}`));
};

const childCache = new Map(); // challengeHex -> {id, genome} (derivation is pure)

// ---- niched selection (fitness sharing) -------------------------------------
//
// Survivor slots go to high-tally sheep, but each pick after the first has its
// tally discounted by similarity to the already-chosen — so one aesthetic
// cannot monopolize a generation no matter how many peers vote for near-
// clones. Deterministic: plain IEEE arithmetic over public genome data, so
// every peer computes the same survivors.

const profileCache = new Map(); // sheepId -> {vars, pal, n}

function profile(record) {
  let p = profileCache.get(record.id);
  if (p) return p;
  const g = JSON.parse(record.genome);
  const vars = new Array(22).fill(0);
  for (const t of g.transforms) {
    t.variations.forEach((w, i) => { vars[i] += Math.abs(w); });
  }
  const sum = vars.reduce((a, b) => a + b, 0) || 1;
  for (let i = 0; i < vars.length; i++) vars[i] /= sum;
  const pal = [0, 1, 2].map((k) =>
    g.palette.stops.reduce((a, s) => a + s.rgb[k], 0) / g.palette.stops.length);
  p = { vars, pal, n: g.transforms.length };
  profileCache.set(record.id, p);
  return p;
}

/** Genome distance in [0, 1]: variation-mix shape, mean palette color,
 *  structural size. */
function distance(a, b) {
  let dv = 0;
  for (let i = 0; i < 22; i++) dv += Math.abs(a.vars[i] - b.vars[i]);
  const dp = (Math.abs(a.pal[0] - b.pal[0]) + Math.abs(a.pal[1] - b.pal[1]) +
    Math.abs(a.pal[2] - b.pal[2])) / 3;
  return Math.min(1, 0.6 * (dv / 2) + 0.3 * dp + 0.1 * (Math.abs(a.n - b.n) / 7));
}

/** Greedy niched pick of up to `k` from `voted` ([record, tally], sorted).
 *  score = tally * (0.25 + 0.75 * min distance to already-chosen). */
function nichedSurvivors(voted, k) {
  const cand = voted.map(([r, c]) => ({ r, c, p: profile(r) }));
  const chosen = [];
  while (chosen.length < k && cand.length) {
    let best = 0;
    let bestScore = -1;
    for (let i = 0; i < cand.length; i++) {
      const minD = chosen.length
        ? Math.min(...chosen.map((s) => distance(cand[i].p, s.p)))
        : 1;
      const score = cand[i].c * (0.25 + 0.75 * minD);
      if (score > bestScore || (score === bestScore && cand[i].r.id < cand[best].r.id)) {
        best = i;
        bestScore = score;
      }
    }
    chosen.push(cand.splice(best, 1)[0]);
  }
  return chosen.map((s) => s.r);
}

/**
 * Replay all generations and return the current living flock.
 *
 * @param store   store.js instance
 * @param baked   gen-0 records from the static manifest
 * @param breedFn async (aJson, bJson, challengeHex) => {childJson, childId}
 * @returns {living: Map(id -> record), genActive: number} — records carry
 *          .derived = true when they were born here rather than gossiped.
 */
export async function computeFlock({
  store, baked, breedFn, mutateFn, randomFn, currentGen, banned = new Set(),
}) {
  const current = currentGen ?? gen();

  // Submissions per generation, after the deterministic per-author cap
  // (lowest sheep ids win — same subset on every peer, any partition).
  const byGen = new Map();
  {
    const byAuthorGen = new Map();
    for (const r of await store.allSheep()) {
      if (r.baked) continue;
      const k = `${r.author}:${r.gen}`;
      if (!byAuthorGen.has(k)) byAuthorGen.set(k, []);
      byAuthorGen.get(k).push(r);
    }
    for (const group of byAuthorGen.values()) {
      group.sort((a, b) => (a.id < b.id ? -1 : 1));
      for (const r of group.slice(0, AUTHOR_GEN_CAP)) {
        if (!byGen.has(r.gen)) byGen.set(r.gen, []);
        byGen.get(r.gen).push(r);
      }
    }
  }

  // Tallies per generation. A sheep's tally is the count of distinct verified
  // batches contributed to it (dedup is inherent: one record per
  // sheep:frame:idx). Each batch is one vote AND one increment of render
  // quality, so "best-looking" and "most-voted" are the same number. Batches
  // from discredited keys (verified fraud proofs) count for nothing.
  const tallyByGen = new Map();
  for (const b of await store.allBatches()) {
    if (banned.has(b.contributor)) continue;
    if (!tallyByGen.has(b.gen)) tallyByGen.set(b.gen, new Map());
    const t = tallyByGen.get(b.gen);
    t.set(b.sheepId, (t.get(b.sheepId) || 0) + 1);
  }

  let living = new Map(baked.map((r) => [r.id, r]));

  // Only generations with activity change anything — skip the quiet ones.
  const eventGens = [...new Set([...byGen.keys(), ...tallyByGen.keys()])]
    .filter((g) => g < current)
    .sort((a, b) => a - b);

  for (const g of eventGens) {
    for (const r of byGen.get(g) || []) living.set(r.id, r); // submissions join their gen
    const tally = tallyByGen.get(g) || new Map();

    const voted = [...living.values()]
      .map((r) => [r, tally.get(r.id) || 0])
      .filter(([, c]) => c > 0)
      .sort((a, b) => b[1] - a[1] || (a[0].id < b[0].id ? -1 : 1));
    // Down-voted to net-negative: culled outright. They cannot survive, fill,
    // or breed this close — death is in the voters' hands too.
    const condemned = new Set(
      [...living.values()].filter((r) => (tally.get(r.id) || 0) < 0).map((r) => r.id));
    if (!voted.length && !condemned.size) continue; // quiet gen: carry over unchanged

    // Voted sheep take survivor slots first — niched, so near-clones share
    // their votes' worth. Remaining slots (of K) fill from the unvoted living,
    // newest first (deterministic). Without the fill, a lone vote would
    // collapse the population to one un-breedable sheep.
    const survivors = nichedSurvivors(voted, SURVIVORS_K);
    if (survivors.length < SURVIVORS_K) {
      const taken = new Set(survivors.map((r) => r.id));
      const fill = [...living.values()]
        .filter((r) => !taken.has(r.id) && !condemned.has(r.id))
        .sort((a, b) => b.gen - a.gen || (a.id < b.id ? -1 : 1))
        .slice(0, SURVIVORS_K - survivors.length);
      survivors.push(...fill);
    }

    // Births: cyclic pairing of survivors; pair (a,b) sorted+deduped so the
    // child set is order-independent.
    const children = [];
    const seenPairs = new Set();
    for (let i = 0; i < survivors.length && survivors.length >= 2; i++) {
      const a = survivors[i];
      const b = survivors[(i + 1) % survivors.length];
      const pair = [a.id, b.id].sort().join();
      if (a.id === b.id || seenPairs.has(pair)) continue;
      seenPairs.add(pair);

      const challengeHex = await breedChallenge(g, a.id, b.id);
      let child = childCache.get(challengeHex);
      if (!child) {
        const { childJson, childId } = await breedFn(a.genome, b.genome, challengeHex);
        child = { id: childId, genome: childJson };
        childCache.set(challengeHex, child);
      }
      children.push({
        id: child.id, genome: child.genome, parents: [a.id, b.id].sort(),
        gen: g + 1, author: null, derived: true, origin: 'pair',
      });
    }

    // Variance injection: high-rate mutant clones of the top survivors
    // (asexual escape from the pair-breeding attractor) and a deterministic
    // immigrant bred from nothing but the generation number — fresh blood
    // arrives every active generation with no user action, and every peer
    // derives the identical genomes.
    if (mutateFn) {
      for (const parent of survivors.slice(0, MUTANTS_PER_GEN)) {
        const challengeHex = await sha256Hex(utf8(`mutant|${g}|${parent.id}`));
        let child = childCache.get(challengeHex);
        if (!child) {
          const { childJson, childId } = await mutateFn(parent.genome, challengeHex, 0.4);
          child = { id: childId, genome: childJson };
          childCache.set(challengeHex, child);
        }
        children.push({
          id: child.id, genome: child.genome, parents: [parent.id],
          gen: g + 1, author: null, derived: true, origin: 'mutant',
        });
      }
    }
    if (randomFn) {
      for (let i = 0; i < IMMIGRANTS_PER_GEN; i++) {
        const challengeHex = await sha256Hex(utf8(`immigrant|${g}|${i}`));
        let child = childCache.get(challengeHex);
        if (!child) {
          const seed = parseInt(challengeHex.slice(0, 8), 16);
          const { childJson, childId } = await randomFn(seed);
          child = { id: childId, genome: childJson };
          childCache.set(challengeHex, child);
        }
        children.push({
          id: child.id, genome: child.genome, parents: null,
          gen: g + 1, author: null, derived: true, origin: 'immigrant',
        });
      }
    }

    living = new Map([...survivors, ...children].map((r) => [r.id, r]));
    for (const id of condemned) living.delete(id); // the swarm said no
  }

  // Submissions released in the current (still open) generation join the view.
  for (const r of byGen.get(current) || []) living.set(r.id, r);

  return { living, genActive: current };
}
