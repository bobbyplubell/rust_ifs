// names.js — deterministic readable handles from pubkeys, and sheep
// provenance lines. A handle is display-sugar only: identity IS the key
// (handles can collide harmlessly; the hex tail disambiguates visually).

import { SURVIVORS_K, GENESIS_GEN } from './net.js';

const ADJ = [
  'amber', 'ashen', 'bold', 'briny', 'calm', 'coral', 'dusky', 'eager',
  'feral', 'gilt', 'hazy', 'icy', 'jade', 'keen', 'lucid', 'mellow',
  'misty', 'neon', 'opal', 'pale', 'quick', 'rosy', 'sable', 'shy',
  'sleek', 'solar', 'tidal', 'umber', 'vivid', 'warm', 'wild', 'zesty',
];
const ANIMAL = [
  'auk', 'bat', 'crane', 'dove', 'eel', 'fox', 'gnu', 'heron',
  'ibis', 'jay', 'kite', 'lark', 'mole', 'newt', 'orca', 'pika',
  'quail', 'rook', 'seal', 'tern', 'urchin', 'vole', 'wren', 'yak',
  'zebu', 'lynx', 'hare', 'toad', 'swift', 'finch', 'moth', 'koi',
];

/** e.g. "misty-heron-3fa9" — pure function of the pubkey hex. */
export function handle(pubHex) {
  const a = parseInt(pubHex.slice(0, 2), 16) % 32;
  const b = parseInt(pubHex.slice(2, 4), 16) % 32;
  return `${ADJ[a]}-${ANIMAL[b]}-${pubHex.slice(4, 8)}`;
}

/**
 * A sheep's UNIQUE display name — same adjective-animal-hex flavor as handle()
 * (so sheep names feel of-a-piece with peer names), but SEEDED FROM THE SHEEP'S
 * CONTENT-ADDRESSED id instead of a pubkey. Because the id is unique per sheep
 * the name is unique; because it's a pure function of the id, every peer shows
 * the same name with no syncing. Display-only sugar: identity IS the id (the hex
 * tail disambiguates the rare adjective/animal collision visually).
 */
export function sheepName(record) {
  // Accept either a record or a bare id string.
  const id = typeof record === 'string' ? record : record?.id;
  if (!id) return 'unknown';
  const a = parseInt(id.slice(0, 2), 16) % 32;
  const b = parseInt(id.slice(2, 4), 16) % 32;
  return `${ADJ[a]}-${ANIMAL[b]}-${id.slice(4, 8)}`;
}

/** Short + long provenance for a sheep record. */
export function provenance(record) {
  const g = record.gen - GENESIS_GEN;
  // Two-parent (crossover) records render a pair; mutants carry a single
  // parent and immigrants carry none, so guard the second slot.
  const pair = record.parents && record.parents.length >= 2
    ? `${record.parents[0].slice(0, 8)} × ${record.parents[1].slice(0, 8)}`
    : null;
  if (record.baked) {
    return { who: record.name || 'seed flock', how: 'seed flock · generation 0' };
  }
  if (record.derived) {
    if (record.origin === 'mutant') {
      return {
        who: `mutant g${g}`,
        how: `born by mutation in generation ${g}: a high-rate mutant clone ` +
          `of top survivor ${record.parents[0].slice(0, 8)}`,
      };
    }
    if (record.origin === 'immigrant') {
      return {
        who: `immigrant g${g}`,
        how: `arrived in generation ${g}: a fresh random genome derived ` +
          'deterministically from the generation number — no parents, no author',
      };
    }
    return {
      who: `selection g${g}`,
      how: `born by natural selection in generation ${g}: survivor pairing ` +
        `(top-${SURVIVORS_K} by vote, cyclic) of ${pair}`,
    };
  }
  if (record.author) {
    const h = handle(record.author);
    return {
      who: h,
      how: `bred & released by ${h} [${record.author.slice(0, 12)}…] in generation ${g}` +
        (pair ? ` from ${pair}` : ''),
    };
  }
  return { who: 'unknown', how: 'unknown origin' };
}
