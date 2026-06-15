// Deployment config (v2 — coordinator architecture).
//
// The static client (GitHub Pages) talks to the coordinator over plain HTTPS:
// it polls GET /api/flock + GET /api/me for live state and POSTs render
// results / votes / breed proposals. No WebSocket, no SSE, no libp2p.
//
// COORDINATOR is the API base URL — everything in API.md hangs off `${COORDINATOR}/api/...`.
// Default below is the local dev coordinator; point it at the production VPS
// (behind Cloudflare) for the deployed site, e.g.
//   export const COORDINATOR = 'https://api.proof-of-sheep.com';
export const COORDINATOR = 'http://localhost:8080';
