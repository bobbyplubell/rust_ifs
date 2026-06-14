# Deploying the relay (public cross-machine swarm)

A browser swarm needs one tiny always-on relay: it can't render or vote, it
only introduces peers — it relays discovery beacons and WebRTC signaling so
browsers can find each other and form DIRECT links, but sheep data then flows
browser-to-browser over WebRTC and never through the relay (so the relay isn't
a bandwidth bottleneck as the swarm grows). Browsers on an https page can only
open `wss://`, so the relay runs behind Caddy (TLS termination).

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

## Running a COMMUNITY relay (add capacity to an existing swarm)

Anyone can add a relay to an existing swarm — no site change, no permission, no
coordination with the maintainer. The new relay **dials into the backbone** and
is then discovered by every browser automatically.

How it works: relays gossip their own address on a relay-discovery topic, and
(this is the key part) **relays dial each other**. Set `BOOTSTRAP` to any
existing relay's multiaddr and your relay joins that relay's mesh; it then
advertises itself, browsers on the swarm hear the ad and dial you, and your
relay starts carrying its share. The backbone self-assembles as more community
relays come online — each one only needs to know *one* existing relay.

```bash
# Same as a normal relay (your own subdomain + cert), PLUS BOOTSTRAP:
cd <repo>/relay/deploy
RELAY_DOMAIN=relay.YOURDOMAIN.com \
BOOTSTRAP=/dns4/relay.proof-of-sheep.com/tcp/443/wss/p2p/12D3KooWMfGMj9QJPdfQopxN18tUbBDpCmxJG6v3EmxMS4EPN4MU \
docker compose up -d --build
```

You still need your **own** subdomain + DNS A record + open 80/443 (browsers on
the https site can only dial `wss://`, so every relay needs its own TLS cert;
Caddy auto-provisions it). Your relay generates its own peer id on first boot —
do **not** reuse another relay's key.

That's it. You do **not** edit `web/config.js` — that list is just the *bootstrap*
relays a fresh browser tries first; community relays are found via gossip. Verify
you joined:
```bash
docker compose logs relay | grep -E "peer id|\[\+\] open"   # should connect out to the bootstrap relay
```

Why this is safe to let strangers do: the relay holds **no authority**. It
forwards only signed, independently-verifiable facts and sees only ciphertext,
so a hostile community relay can withhold or delay data but can never forge a
sheep, a vote, or a render. More relays = more resilience, never less trust.

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
