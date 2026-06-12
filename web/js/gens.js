// gens.js — the generation engine: a pure function from the fact store to the
// living flock. Nothing here is gossiped or signed; every peer recomputes the
// same lineage from the same facts (children are derived via the deterministic
// wasm breeder), so agreement needs no consensus.
//
// Population pressure (the point of generations): survivors are a fixed top-K
// by tally, automatic births are bounded by K, submissions cost a render proof
// and are capped per (author, generation) — so the flock cannot blow up with
// peer count. Quiet generations (no votes) carry the flock forward unchanged.

import { gen, SURVIVORS_K, AUTHOR_GEN_CAP } from './net.js';
import { sha256Hex, utf8 } from './hash.js';

// Canonical child challenge for a pair in generation g (ids sorted, so a pair
// has one child regardless of who computes it). Also used by the nursery
// preview in app.js — the preview IS the canonical child.
export const breedChallenge = (g, idA, idB) => {
  const [x, y] = [idA, idB].sort();
  return sha256Hex(utf8(`breed|${g}|${x}|${y}`));
};

const childCache = new Map(); // challengeHex -> {id, genome} (derivation is pure)

/**
 * Replay all generations and return the current living flock.
 *
 * @param store   store.js instance
 * @param baked   gen-0 records from the static manifest
 * @param breedFn async (aJson, bJson, challengeHex) => {childJson, childId}
 * @returns {living: Map(id -> record), genActive: number} — records carry
 *          .derived = true when they were born here rather than gossiped.
 */
export async function computeFlock({ store, baked, breedFn, currentGen }) {
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

  // Vote tallies per generation (dedup is inherent: one key per voter:sheep:gen).
  const tallyByGen = new Map();
  for (const v of await store.allVotes()) {
    if (!tallyByGen.has(v.gen)) tallyByGen.set(v.gen, new Map());
    const t = tallyByGen.get(v.gen);
    t.set(v.sheepId, (t.get(v.sheepId) || 0) + 1);
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
    if (!voted.length) continue; // quiet generation: carry over unchanged

    // Voted sheep take survivor slots first; remaining slots (of K) fill from
    // the unvoted living, newest first (deterministic). Without the fill, a
    // lone vote would collapse the population to one un-breedable sheep.
    const survivors = voted.slice(0, SURVIVORS_K).map(([r]) => r);
    if (survivors.length < SURVIVORS_K) {
      const taken = new Set(survivors.map((r) => r.id));
      const fill = [...living.values()]
        .filter((r) => !taken.has(r.id))
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
        gen: g + 1, author: null, derived: true,
      });
    }

    living = new Map([...survivors, ...children].map((r) => [r.id, r]));
  }

  // Submissions released in the current (still open) generation join the view.
  for (const r of byGen.get(current) || []) living.set(r.id, r);

  return { living, genActive: current };
}
