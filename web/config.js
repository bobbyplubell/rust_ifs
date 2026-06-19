// Deployment config (v3 — node HTTP API).
//
// The static client (GitHub Pages) talks to a sheep-node over plain HTTPS: it
// polls GET /api/flock for live state, GET /api/assign for work, and POSTs
// signed Envelopes to /api/msg (renders / votes / births). No WebSocket, no
// SSE, no libp2p in the browser (the node bridges writes into the swarm).
//
// WORLD SELECTION ------------------------------------------------------------
// A "world" is one node (one flock). The client can point at any of a few
// known worlds or a custom URL. The active world is resolved, in order:
//   1. `?world=<url>` query param — a shareable deep-link to a specific world,
//   2. localStorage (`sheep-world`) — the user's last pick, persisted,
//   3. the first WORLDS entry — the default.
// A `?world=` value is also written back to localStorage so it sticks.
//
// The IDENTITY (the Ed25519 key in identity.js) is SHARED across all worlds —
// they're the same Pages origin, so the same localStorage key. Only per-world
// standing (credits / reputation / tiles) differs. The UI notes this.
//
// COORDINATOR is the resolved API base URL — everything in API.md hangs off
// `${COORDINATOR}/api/...`.

// Known worlds. The prod URLs are placeholders the deploy fills in (point them
// at the production VPS behind Cloudflare). 'Local dev' is first, so the
// default (no ?world=, nothing stored) stays the dev coordinator; for a
// production deploy, move the prod world to the front of this list.
export const WORLDS = [
  { name: 'Sandbox', url: 'https://relay.proof-of-sheep.com' },   // relay1 droplet — fast/wild (5min, high mutation)
  { name: 'Gallery', url: 'https://relay2.proof-of-sheep.com' },  // relay2 droplet — slow/refined (1h, low mutation)
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
