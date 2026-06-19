# Deploying a proof-of-sheep v3 world (a `sheep-node` seed)

This directory runs **one `sheep-node` "world" in server mode** — a *seed*
(ARCHITECTURE v3 §1): an always-on libp2p **bootstrap + relay** that also runs
the **HTTP read/write face** for browsers — behind Caddy (auto-HTTPS) with
Docker Compose. The SAME files run either world; a world is just a domain plus a
decay/economy "personality". See `ARCHITECTURE-v3.md` §2.2 for the
Sandbox-vs-Gallery idea and §12-step-9 for the deploy shape.

> **The actual deploy is yours to run.** These are config artifacts only. The
> deploy touches your live droplets, GHCR, and Pages — nothing here builds,
> pushes, or deploys on its own.

## Files

| file                          | role                                                                 |
|-------------------------------|----------------------------------------------------------------------|
| `Dockerfile`                  | multi-stage Rust build of `sheep-node`; runtime image includes `ffmpeg` |
| `docker-compose.yml`          | build-from-source: seed + Caddy, parameterized by env (one file, either world) |
| `docker-compose.prebuilt.yml` | pulls the GHCR image instead of building — for the droplets           |
| `Caddyfile`                   | `{$WORLD_DOMAIN}` auto-HTTPS, `protocols h1`, proxies the node's HTTP face |
| `sandbox.env`                 | the FAST / EPHEMERAL world (steep decay, cheap births)               |
| `gallery.env`                 | the SLOW / CURATED world (gentle decay, dearer births)              |
| `../../../.github/workflows/sheep-node-image.yml` | CI: build amd64 image → GHCR        |

The Dockerfile build context is the **repo root** (it copies the whole
workspace, since `sheep-node` path-depends on `crates/sheep-proto` +
`crates/flame-core`); the compose files set `context: ../../..` accordingly. Run
compose from *this* `deploy/` dir.

## How it maps to the v2 coordinator deploy

This is adapted from `coordinator/deploy/`. Key differences:

- The service is `sheep-node` (not `coordinator`), and it is **also a libp2p
  peer**, so its swarm port (`4001`) is **published directly** (peers must dial
  it). The HTTP face (`8080`) stays unpublished behind Caddy, as before.
- The Caddyfile keeps a **`servers { protocols h1 }`** pin (the v1 wss lesson) —
  harmless today (browser is HTTP-only), future-proofs a wss bootstrap.
- A world's personality is **age-escalating decay + economy knobs** (v3 has no
  generations), not the v2 GA `GEN_MS`/mutation knobs.
- The one piece of non-regenerable state is the **persisted node identity key**
  (it fixes the seed's peer id), not a SQLite `account` table.

## The image pipeline (CI → GHCR → pull)

The droplets are x86_64 and shouldn't compile Rust. So, exactly as in v2:

1. A push touching `crates/**`, `web/genomes/**`, or the workflow triggers
   `.github/workflows/sheep-node-image.yml`.
2. GitHub's native amd64 runner builds `crates/sheep-node/deploy/Dockerfile` and
   pushes `ghcr.io/<owner>/sheep-node:latest` + `:<sha>`.
3. Each droplet `docker pull`s and runs the **prebuilt** compose file.

## Option A — one world per droplet

Each world gets its own droplet and subdomain. Two droplets → two worlds, and
**each is the other's bootstrap peer** (the swarm spans both worlds).

### 1. DNS

Point an **A record** at each droplet's public IP, per world:

```
sandbox   A   <sandbox-droplet-ip>
gallery   A   <gallery-droplet-ip>
```

(The droplet IPs are deliberately not in this repo. Fill them in your DNS panel.)
Wait for the record to resolve before starting Caddy (it needs the name to
resolve to *this* box to complete the Let's Encrypt HTTP-01 challenge on :80).

### 2. Pull + bring the world up (the exact droplet invocation)

From this `deploy/` dir on the target droplet:

```sh
docker pull ghcr.io/<owner>/sheep-node:latest

# Sandbox droplet:
docker compose -f docker-compose.prebuilt.yml --env-file sandbox.env up -d

# Gallery droplet:
docker compose -f docker-compose.prebuilt.yml --env-file gallery.env up -d
```

`WORLD_DOMAIN` is set inside each env file (both example files set it), so it
doesn't need to be on the command line; pass it explicitly if you prefer:
`WORLD_DOMAIN=sandbox.proof-of-sheep.com docker compose -f docker-compose.prebuilt.yml --env-file sandbox.env up -d`.

Caddy provisions a real Let's Encrypt cert on first request and serves the HTTP
face at `https://<WORLD_DOMAIN>/`. The node's data — the **persisted identity
key** + the regenerable hist/video caches — persists in the `sheep-data` volume.

(To build from source on a beefier box instead: swap
`-f docker-compose.prebuilt.yml` for the default `docker-compose.yml` and add
`--build`.)

## The bootstrap list (the two-seed story)

Browsers never speak libp2p — they hit the HTTP face. But the **native swarm**
(peers, and the two seeds themselves) is wired by a **bootstrap list**: each
seed's stable public multiaddr.

A seed's peer id is derived from its **identity key**, which is generated on
first run and persisted in the `sheep-data` volume. So:

1. **First boot** each world *without* `SHEEP_BOOTSTRAP` set (the env files ship
   it blank). Each seed generates + persists its key and **prints its public key
   / peer id** to the log (`sheep-node: pub=<hex> listen=… …`).

   ```sh
   docker compose -f docker-compose.prebuilt.yml --env-file sandbox.env logs sheep-node | grep '^sheep-node: pub='
   ```

2. **Form each world's bootstrap multiaddr** from its domain + swarm port + peer
   id. The intended stable form (once the node persists its key and advertises a
   dialable address) is:

   ```
   /dns4/sandbox.proof-of-sheep.com/tcp/4001/p2p/<sandbox-peerid>
   /dns4/gallery.proof-of-sheep.com/tcp/4001/p2p/<gallery-peerid>
   ```

   (If/when a `wss` listen is added behind Caddy, the form becomes
   `/dns4/<domain>/tcp/443/wss/p2p/<peerid>` — the `protocols h1` pin in the
   Caddyfile is there precisely so that upgrade isn't broken by HTTP/2.)

3. **Cross-wire:** put the *other* world's multiaddr into each env file's
   `SHEEP_BOOTSTRAP` (space-separated if more than one) and bring the world back
   up so each seed dials the other:

   ```sh
   # in sandbox.env:  SHEEP_BOOTSTRAP=/dns4/gallery.…/tcp/4001/p2p/<gallery-peerid>
   # in gallery.env:  SHEEP_BOOTSTRAP=/dns4/sandbox.…/tcp/4001/p2p/<sandbox-peerid>
   docker compose -f docker-compose.prebuilt.yml --env-file sandbox.env up -d
   ```

The peer ids are stable **as long as the `sheep-data` volume (the key) survives**
— that's why the key is the one thing to back up. Lose it and the seed comes back
with a new peer id, breaking the published bootstrap multiaddr until you re-share
it.

## Pages serves the web client

The browser client (`web/`) is served by **GitHub Pages**
(`.github/workflows/pages.yml`), pointed at the two worlds' HTTPS domains
(the world picker). Pages is static; it talks only to the seeds' HTTP read/write
faces over HTTPS — no libp2p, no wss in the browser (ARCHITECTURE v3 §10).

## World personalities

Both worlds run the identical binary; only the env differs.

| knob                          | sandbox (fast/ephemeral) | gallery (slow/curated) | engine default            |
|-------------------------------|-------------------------:|-----------------------:|---------------------------|
| `SHEEP_DECAY_TIME_UNIT_MS`    | 1000                     | 3600000                | 1000                      |
| `SHEEP_DECAY_QUAD`            | 1.0                      | 0.25                   | 0.25                      |
| `SHEEP_DECAY_EXP_SCALE`       | 2.0                      | 1.0                    | 1.0                       |
| `SHEEP_DECAY_HALF_LIFE`       | 4.0                      | 12.0                   | 8.0                       |
| `SHEEP_MINT_COST`             | 4                        | 12                     | 8                         |
| `SHEEP_BREED_COST`            | 12                       | 30                     | 20                        |

Steep decay (Sandbox) = sheep age out fast (churn); gentle decay (Gallery) =
long-lived, curated. The exponential tail guarantees turnover in both (§2.2).

## TODO — pending binary flags

The current `crates/sheep-node/src/main.rs` accepts ONLY:

```
sheep-node --listen <multiaddr> [--bootstrap <multiaddr>]... [--key <64-hex>]
```

These artifacts are authored against the *documented server-mode interface*
(ARCHITECTURE v3 §1/§12). The following are **not wired in the binary yet** and
must be added before the deploy is fully functional. They are clearly fenced in
the compose/env files so they activate the moment the binary supports them:

- **`--http-addr <addr>`** — the HTTP read/write face (the entire browser/watch
  face). Without it the seed is a pure libp2p peer; Caddy has nothing to proxy.
  Compose passes it via `SHEEP_HTTP_ADDR`; the CMD does **not** yet append
  `--http-addr` (the flag doesn't exist), so add that to the compose `command`
  when it lands.
- **`--key <path>` (key FILE persistence)** + a **data dir**. Today `--key`
  takes a 64-char *hex secret* (ephemeral if omitted), so the **peer id is NOT
  stable across restarts** — which breaks the bootstrap multiaddr. The compose
  uses a `sheep-data` volume in anticipation; until the binary persists its key
  to `/data`, pin identity by passing a fixed hex secret via a (secret) env, or
  accept a fresh peer id each restart. **Do not commit a real key.**
- **Env-driven decay / economy knobs.** `engine.rs` has `DecayParams`,
  `HallThreshold`, `Engine::set_decay_params()`, and `spec.rs` has the cost
  constants — but nothing reads the `SHEEP_DECAY_*` / `SHEEP_HALL_*` /
  `SHEEP_*_COST` env vars yet. Setting them today is a **no-op**. Wire them into
  the binary's startup (read env → `set_decay_params` etc.) to make worlds
  differ.
- **Genomes dir / video serving.** `GENOMES_DIR` + `/api/video/*` mirror v2; the
  genesis sheep is derived in-binary so genomes aren't required to boot, but the
  HTTP face's read projections (flock snapshot + video) are part of the same TODO
  as `--http-addr`.

## Backups — the identity key

Unlike v2 (which backed up the SQLite `account` table), v3's credits/reputation
are **log-derived** (recomputed from the gossiped attestation log), so they are
regenerable. The accumulated histograms + videos are also regenerable
(deterministic re-render). The single non-regenerable thing is each seed's
**persisted identity key** in the `sheep-data` volume — it fixes the peer id the
published bootstrap multiaddr depends on. Once the binary persists its key,
back up that key file (e.g. cross-rsync to the other droplet). Until then there's
no on-disk key to back up (it's ephemeral — see TODO).

## Operations

```sh
# tail logs
docker compose -f docker-compose.prebuilt.yml --env-file sandbox.env logs -f sheep-node

# update to a freshly-built image
docker pull ghcr.io/<owner>/sheep-node:latest
docker compose -f docker-compose.prebuilt.yml --env-file sandbox.env up -d

# stop (data + caddy certs survive in their volumes)
docker compose -f docker-compose.prebuilt.yml --env-file sandbox.env down
```
