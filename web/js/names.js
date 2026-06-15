// names.js — deterministic readable handles from pubkeys, and sheep
// provenance lines. A handle is display-sugar only: identity IS the key
// (handles can collide harmlessly; the hex tail disambiguates visually).

// v2: the coordinator owns the flock, so generations are absolute (no
// genesis-offset replay) — gen 0 is the seed flock. The old net.js constants
// (SURVIVORS_K / GENESIS_GEN) retired with the P2P plumbing; inline what's left.
const GENESIS_GEN = 0;

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

/**
 * Short + long provenance for a sheep record.
 *
 * v2: the coordinator owns the flock and hands the client a flat record
 * ({ id, name, parents:[idA,idB]|null, gen, ... }). The rich v1 origin
 * taxonomy (baked / mutant / immigrant / authored) lived in client-side
 * replay state that no longer exists, so provenance now derives purely from
 * `parents` + `gen`: gen 0 (or no parents) = seed flock; two parents = a bred
 * pairing.
 */
export function provenance(record) {
  const g = (record.gen ?? 0) - GENESIS_GEN;
  const parents = Array.isArray(record.parents) ? record.parents.filter(Boolean) : [];
  const pair = parents.length >= 2
    ? `${parents[0].slice(0, 8)} × ${parents[1].slice(0, 8)}`
    : null;
  if (g <= 0 || !parents.length) {
    return { who: record.name || 'seed flock', how: 'seed flock · generation 0' };
  }
  return {
    who: `generation ${g}`,
    how: pair
      ? `bred in generation ${g} from the pairing of ${pair}`
      : `born in generation ${g}`,
  };
}
