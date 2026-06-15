# Deploying a proof-of-sheep world

This directory runs **one coordinator "world"** behind Caddy (auto-HTTPS) with
Docker Compose. The SAME files run either world — a world is just a domain plus
a generation length plus a GA "personality" (mutation / immigrants / selection).
See `ARCHITECTURE.md` for the Sandbox-vs-Gallery idea.

## Files

| file                  | role                                                              |
|-----------------------|-------------------------------------------------------------------|
| `docker-compose.yml`  | coordinator + Caddy; parameterized by env (one file, either world)|
| `Caddyfile`           | `{$WORLD_DOMAIN} { reverse_proxy coordinator:8080 }`, auto-HTTPS   |
| `sandbox.env`         | the FAST / WILD world (short gens, high mutation + immigrants)     |
| `gallery.env`         | the SLOW / REFINED world (long gens, low mutation + immigrants)    |
| `backup-accounts.sh`  | cron-able dump+rsync of ONLY the `account` table (the one backup)  |
| `../Dockerfile`       | multi-stage Rust build; runtime image includes `ffmpeg`           |

The Dockerfile build context is the **repo root** (it copies the whole
workspace, since `coordinator` path-depends on `crates/flame-core`); the compose
file sets `context: ../..` accordingly. Run compose from *this* `deploy/` dir.

## Option A — one world per droplet (no failover)

Each world gets its own droplet and its own subdomain. Simplest topology; no
floating IP, no standby. (The two repurposed droplets — `178.128.157.72`,
`174.138.34.46` — can host one world each, and back each other's `account`
table up; see Backups.)

### 1. DNS

Point an **A record** at the droplet's public IP, per world:

```
sandbox   A   178.128.157.72      # the Sandbox droplet
gallery   A   174.138.34.46       # the Gallery droplet
```

Wait for the record to resolve before starting Caddy (it needs the name to
resolve to *this* box to complete the Let's Encrypt HTTP-01 challenge on :80).

### 2. Bring the world up

From this `deploy/` dir on the target droplet:

```sh
# Sandbox droplet:
WORLD_DOMAIN=sandbox.proof-of-sheep.com \
  docker compose --env-file sandbox.env up -d --build

# Gallery droplet:
WORLD_DOMAIN=gallery.proof-of-sheep.com \
  docker compose --env-file gallery.env up -d --build
```

`WORLD_DOMAIN` is read by both Caddy (the site address it provisions HTTPS for)
and is passed alongside the env file's `GEN_MS` + GA knobs. You can instead set
`WORLD_DOMAIN=` inside the env file (both example files do) and drop it from the
command line.

Caddy provisions a real Let's Encrypt cert on first request and serves the world
at `https://<WORLD_DOMAIN>/`. The coordinator data (SQLite + the regenerable
hist/video caches) persists in the `coordinator-data` Docker volume.

### 3. CORS

CORS is handled **in the coordinator** (`tower_http::cors`, `allow_origin(Any)`),
not in Caddy — so the static GitHub Pages client (a separate origin) can call the
API directly. Caddy just proxies; no CORS config needed there.

### Verify

```sh
curl -s https://<WORLD_DOMAIN>/health | jq
docker compose --env-file sandbox.env logs coordinator | grep "GA config"
```

The boot log line `GA config: mutation_rate=… immigrants=… survivors=…` confirms
the world picked up its personality (Sandbox and Gallery show different numbers).

## World personalities

Both worlds run the identical binary; only the env differs. Unset knobs fall back
to the code defaults (which equal today's hardcoded behavior).

| knob                    | default | sandbox (fast/wild) | gallery (slow/refined) |
|-------------------------|--------:|--------------------:|-----------------------:|
| `GEN_MS`                | 86400000 (24h) | 300000 (5m)  | 3600000 (1h)           |
| `GA_MUTATION_RATE`      | 0.15    | 0.45                | 0.07                   |
| `GA_MUTATION_MAGNITUDE` | 1       | 2                   | 1                      |
| `GA_IMMIGRANTS`         | 2       | 4                   | 1                      |
| `GA_SURVIVORS`          | 3       | 2                   | 4                      |
| `GA_FLOCK_SIZE`         | 8       | 10                  | 8                      |

## Backups — the `account` table only

Everything else is regenerable (histograms + videos rebuild from canonical
state, the flock re-evolves), so the only backup is the `account` table:
credits, reputation, tile tallies, bans, nonces — keyed by contributor pubkey.

`backup-accounts.sh` dumps just that table (`sqlite3 <db> ".dump account"`) and
rsyncs the dump dir to a peer host. Run it from cron every few minutes. With two
droplets, each world backs up to the other (cross-replication), so either box can
restore the other's accounts.

```sh
# crontab -e on the Sandbox droplet (ship to the Gallery droplet):
*/5 * * * * DB=/var/lib/docker/volumes/deploy_coordinator-data/_data/coordinator.sqlite \
            PEER=backup@174.138.34.46:/srv/sheep-backups/sandbox \
            /opt/wasm-sheep/coordinator/deploy/backup-accounts.sh \
            >> /var/log/sheep-backup.log 2>&1
```

Knobs: `DB` (sqlite path — defaults to the `deploy_coordinator-data` volume),
`PEER` (rsync dest; if unset the script keeps a local dump only), `OUT_DIR`
(local dump dir, default `/var/backups/sheep`), `KEEP` (recent dumps to retain,
default 30). The dump uses a read-only URI so it never blocks the live writer.

Restore:

```sh
sqlite3 /path/to/coordinator.sqlite < accounts-<ts>.sql
```

## Operations

```sh
# tail logs
docker compose --env-file sandbox.env logs -f coordinator

# restart after a code change (rebuilds the image)
WORLD_DOMAIN=sandbox.proof-of-sheep.com \
  docker compose --env-file sandbox.env up -d --build

# stop (data + caddy certs survive in their volumes)
docker compose --env-file sandbox.env down
```
