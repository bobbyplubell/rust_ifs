// Deployment config. RELAYS empty = local-only mode (BroadcastChannel between
// same-origin tabs). To join the internet swarm, list relay multiaddrs
// (wss:// when the site is served over https), e.g.:
//   '/dns4/relay.example.com/tcp/443/wss/p2p/12D3KooW...'
// The libp2p bundle (web/js/vendor/libp2p.js, ~620 KB) is only fetched when
// this list is non-empty.
export const RELAYS = [
  '/dns4/relay.proof-of-sheep.com/tcp/443/wss/p2p/12D3KooWMfGMj9QJPdfQopxN18tUbBDpCmxJG6v3EmxMS4EPN4MU',
];
