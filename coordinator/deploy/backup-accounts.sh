#!/usr/bin/env bash
#
# backup-accounts.sh — the ONLY backup this deployment needs.
#
# Per ARCHITECTURE §2/§6, everything else on a world is regenerable: the
# accumulated histograms and the videos rebuild from the canonical state, and
# the flock/votes/tiles can be re-derived or simply re-evolved. The single piece
# of state that represents real, NON-regenerable user value is the `account`
# table — credits, reputation, tile tallies, ban flags, nonces — keyed by each
# contributor's Ed25519 pubkey. Lose it and contributors lose their standing.
#
# So this dumps ONLY the `account` table and rsyncs it to a peer host. Meant to
# run from cron every few minutes (see DEPLOY.md). Cheap and idempotent.
#
# Config (all via env; sensible defaults for the Docker layout):
#   DB        path to the world's SQLite db
#             (default: the coordinator-data volume's coordinator.sqlite)
#   PEER      rsync destination, e.g. backup@174.138.34.46:/srv/sheep-backups/sandbox
#             (REQUIRED — the script exits 0 with a warning if unset, so a
#              misconfigured cron line doesn't spam failures.)
#   OUT_DIR   local dir to write the dump before rsync (default: /var/backups/sheep)
#   KEEP      how many recent local dumps to retain (default: 30)
#   SSH_OPTS  extra ssh options for rsync (default: "-o StrictHostKeyChecking=accept-new")
#
# Example cron (every 5 min):
#   */5 * * * * DB=/var/lib/docker/volumes/deploy_coordinator-data/_data/coordinator.sqlite \
#               PEER=backup@174.138.34.46:/srv/sheep-backups/sandbox \
#               /path/to/backup-accounts.sh >> /var/log/sheep-backup.log 2>&1

set -euo pipefail

DB="${DB:-/var/lib/docker/volumes/deploy_coordinator-data/_data/coordinator.sqlite}"
OUT_DIR="${OUT_DIR:-/var/backups/sheep}"
KEEP="${KEEP:-30}"
SSH_OPTS="${SSH_OPTS:--o StrictHostKeyChecking=accept-new}"

if [ ! -f "$DB" ]; then
  echo "backup-accounts: db not found at $DB — nothing to back up" >&2
  exit 1
fi

mkdir -p "$OUT_DIR"
ts="$(date -u +%Y%m%dT%H%M%SZ)"
out="$OUT_DIR/accounts-$ts.sql"

# Dump ONLY the account table. `.dump <table>` emits the CREATE + INSERTs for
# just that table, so the file is tiny and restore-able with `sqlite3 db < file`.
# Use a read-only URI so we never block the live coordinator's writes (WAL).
sqlite3 "file:$DB?mode=ro" ".dump account" > "$out"

echo "backup-accounts: wrote $out ($(wc -c < "$out") bytes)"

# Prune old local dumps, keeping the most recent $KEEP.
ls -1t "$OUT_DIR"/accounts-*.sql 2>/dev/null | tail -n "+$((KEEP + 1))" | xargs -r rm -f

if [ -z "${PEER:-}" ]; then
  echo "backup-accounts: PEER unset — kept local dump only (set PEER to replicate)" >&2
  exit 0
fi

# Ship the whole local dump dir to the peer host (idempotent; only changed
# files transfer). Continuous replication is just this on a tight cron.
rsync -az --delete -e "ssh $SSH_OPTS" "$OUT_DIR"/ "$PEER"/
echo "backup-accounts: synced $OUT_DIR -> $PEER"
