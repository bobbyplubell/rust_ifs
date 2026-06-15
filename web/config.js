// Deployment config (v2 — coordinator architecture).
//
// The static client (GitHub Pages) talks to a coordinator over plain HTTPS: it
// polls GET /api/flock + GET /api/me for live state and POSTs render results /
// votes / breed proposals. No WebSocket, no SSE, no libp2p.
//
// WORLD SELECTION ------------------------------------------------------------
// A "world" is one coordinator (one flock). The client can point at any of a
// few known worlds or a custom URL. The active world is resolved, in order:
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
  { name: 'Local dev', url: 'http://localhost:8080' },
  { name: 'Sandbox', url: 'https://sandbox.proof-of-sheep.com' }, // deploy fills in
  { name: 'Gallery', url: 'https://gallery.proof-of-sheep.com' }, // deploy fills in
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
