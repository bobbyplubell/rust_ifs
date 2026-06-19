// Deployment config (v3 — node HTTP API).
//
// The static client (GitHub Pages) talks to a sheep-node over plain HTTPS: it
// polls GET /api/flock for live state, GET /api/assign for work, and POSTs
// signed Envelopes to /api/msg (renders / votes / births). No WebSocket, no
// SSE, no libp2p in the browser (the node bridges writes into the swarm).
//
// GATEWAY SELECTION -----------------------------------------------------------
// There is ONE swarm (one shared flock). The "worlds" list is really a list of
// GATEWAY NODES into that single swarm — relay1 and relay2 are cross-bootstrapped
// peers that converge on the same flock (via gossip + flock-sync), so whichever
// you talk to shows the same sheep. The picker just chooses which node bridges
// your reads/writes (handy as a fallback if one gateway is down). The active
// gateway is resolved, in order:
//   1. `?world=<url>` query param — a shareable deep-link to a specific gateway,
//   2. localStorage (`sheep-world`) — the user's last pick, persisted,
//   3. the first WORLDS entry — the default.
// A `?world=` value is also written back to localStorage so it sticks.
//
// The IDENTITY (the Ed25519 key in identity.js) is SHARED across gateways —
// same Pages origin, same localStorage key — and since it's one swarm your
// credits / reputation / tiles are the same whichever gateway you use.
//
// COORDINATOR is the resolved API base URL — everything in API.md hangs off
// `${COORDINATOR}/api/...`.

// Gateway nodes into the one shared swarm. relay1 is primary; relay2 is a mirror
// (same flock behind both). 'Local dev' is last so the deployed Pages client
// defaults to the primary gateway; a localhost page still auto-picks dev below.
export const WORLDS = [
  { name: 'Flock (relay1)', url: 'https://relay.proof-of-sheep.com' },   // primary gateway — the one shared swarm
  { name: 'Flock (relay2)', url: 'https://relay2.proof-of-sheep.com' },  // mirror gateway — same swarm, same flock
  { name: 'Local dev', url: 'http://localhost:8080' },
];

const WORLD_LS_KEY = 'sheep-world';

function normalize(url) {
  return String(url || '').trim().replace(/\/+$/, '');
}

/** Resolve the active coordinator URL: ?world= → localStorage → first WORLDS. */
export function resolveWorld() {
  const param = normalize(new URLSearchParams(location.search).get('world'));
  if (param) {
    try { localStorage.setItem(WORLD_LS_KEY, param); } catch { /* private mode */ }
    return param;
  }
  let stored = null;
  try { stored = normalize(localStorage.getItem(WORLD_LS_KEY)); } catch { /* ignore */ }
  if (stored) return stored;
  // Host-aware default: a page served from localhost (./dev.sh) defaults to the
  // local coordinator; the deployed Pages client defaults to the first real world.
  const h = location.hostname;
  if (h === 'localhost' || h === '127.0.0.1' || h === '') {
    const dev = WORLDS.find((w) => /localhost|127\.0\.0\.1/.test(w.url));
    if (dev) return normalize(dev.url);
  }
  return normalize(WORLDS[0].url);
}

/** Persist a chosen world URL (the picker calls this, then reloads). */
export function setWorld(url) {
  const u = normalize(url);
  try { localStorage.setItem(WORLD_LS_KEY, u); } catch { /* ignore */ }
  return u;
}

// The resolved API base URL for this page load.
export const COORDINATOR = resolveWorld();
