# Deploying the relay (public cross-machine swarm)

A browser swarm needs one tiny always-on relay: it can't render or vote, it
only introduces peers (and gossips so messages flow before any direct link).
Browsers on an https page can only open `wss://`, so the relay runs behind
Caddy (TLS termination).

## 1. Host + DNS
- Any small VPS with Docker. Open ports 80 and 443 (TCP+UDP).
- Point a DNS record (e.g. `relay.yourdomain.com`) at the host.

## 2. Run it
```bash
git clone <repo> && cd <repo>/relay/deploy
RELAY_DOMAIN=relay.yourdomain.com docker compose up -d --build
```
Caddy auto-provisions a Let's Encrypt cert. Get the relay's peer id:
```bash
docker compose logs relay | grep "peer id"
```

## 3. Point the site at it
The relay multiaddr is:
```
/dns4/relay.yourdomain.com/tcp/443/wss/p2p/<PEER_ID>
```
Either commit it into `web/config.js` `RELAYS = [ ... ]`, or test without
committing by appending `?relay=/dns4/.../wss/p2p/<PEER_ID>` to the site URL.

The site itself must be served over **https** (GitHub Pages is; for a custom
host use `serve-https.sh`) — `crypto.subtle` and `wss://` both require it.

## 4. Full-scale stress test
Two flavours:
- **Same-machine protocol scale** (no relay needed): `stress/run.sh` packs
  hundreds of peers into one Chromium over BroadcastChannel — tests the
  protocol/selection/gossip at scale. `PEERS=400 MINUTES=30 ./stress/run.sh`.
- **Cross-machine / real network**: open the https site with `?relay=...` on
  many machines/tabs; they form a real swarm through this relay. (Driving
  hundreds of *separate* browser contexts through the relay needs a harness
  variant — the BroadcastChannel harness shares one context.)

The relay holds NO authority: every record it forwards is signed and
independently verifiable, so a malicious relay can withhold data but never
forge a sheep, vote, or render.
