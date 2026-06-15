//! SQLite canonical state (WAL mode).
//!
//! The DB holds the *small* state. The big regenerable caches — accumulated
//! per-(sheep,frame) histograms and encoded videos — live as files on disk
//! (see `render.rs` / `video.rs`), reconstructable from the genome + the
//! assignment ledger.

use rusqlite::Connection;
use std::sync::Mutex;

/// A thread-safe handle to the single SQLite connection. SQLite with WAL
/// handles concurrent readers + one writer; we serialize all access behind a
/// Mutex which is plenty for a polling-based, low-QPS coordinator.
pub struct Db {
    pub conn: Mutex<Connection>,
}

impl Db {
    pub fn open(path: &str) -> anyhow::Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let db = Db { conn: Mutex::new(conn) };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(SCHEMA)?;
        // Additive columns for the disk working-cache bookkeeping (idempotent:
        // ignore "duplicate column" on an already-migrated DB).
        for alter in [
            // Unix-ms a sheep's histogram was last merged into — the LRU key for
            // hist eviction. 0 = never merged (no hist on disk).
            "ALTER TABLE sheep ADD COLUMN hist_touched_ms INTEGER NOT NULL DEFAULT 0",
            // Cached on-disk byte size of this sheep's hist/<id>/ dir. 0 when
            // evicted/absent; lets us total usage without statting every file.
            "ALTER TABLE sheep ADD COLUMN hist_bytes INTEGER NOT NULL DEFAULT 0",
            // Peer-audit state of an accepted tile:
            //   0 = unaudited (trust-ingested; merged but not yet peer-verified)
            //   1 = audited   (a peer re-rendered it and the hash matched)
            // The `hash` column already holds the tile's content hash (now the
            // hash of the UPLOADED pixels, not a server re-render).
            "ALTER TABLE tile ADD COLUMN audit_status INTEGER NOT NULL DEFAULT 0",
            // Reputation-weighted audit bookkeeping on the account: how many of
            // this key's tiles have been audited, and how many audits it has
            // performed correctly — both feed the trust math.
            "ALTER TABLE account ADD COLUMN audits_done INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE account ADD COLUMN audits_correct INTEGER NOT NULL DEFAULT 0",
        ] {
            match conn.execute(alter, []) {
                Ok(_) => {}
                Err(rusqlite::Error::SqliteFailure(_, Some(ref m)))
                    if m.contains("duplicate column name") => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }
}

/// The full schema. Idempotent (IF NOT EXISTS) so it doubles as the migration.
pub const SCHEMA: &str = r#"
-- Server-wide singleton state (one row, id=0): current generation + its start.
CREATE TABLE IF NOT EXISTS meta (
    id            INTEGER PRIMARY KEY CHECK (id = 0),
    gen           INTEGER NOT NULL DEFAULT 0,
    gen_started_ms INTEGER NOT NULL,   -- unix ms when this gen opened
    gen_ms        INTEGER NOT NULL     -- gen length (mirrors spec::GEN_MS)
);

-- A sheep = a genome + lineage + lifecycle. `id` is the flame-core sheep_id
-- (64-hex sha256 of canonical genome JSON).
CREATE TABLE IF NOT EXISTS sheep (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    genome      TEXT NOT NULL,          -- canonical genome JSON
    parent_a    TEXT,                   -- sheep_id or NULL (immigrant/genesis)
    parent_b    TEXT,
    gen         INTEGER NOT NULL,       -- generation born
    n_frames    INTEGER NOT NULL,       -- this sheep's loop length (spec)
    w           INTEGER NOT NULL,
    h           INTEGER NOT NULL,
    ss          INTEGER NOT NULL,
    spp         INTEGER NOT NULL,
    alive       INTEGER NOT NULL DEFAULT 1,  -- 0 = culled
    hof         INTEGER NOT NULL DEFAULT 0,  -- 1 = enshrined (hall of fame)
    tiles       INTEGER NOT NULL DEFAULT 0,  -- total accepted tiles
    created_ms  INTEGER NOT NULL,
    video_rev   INTEGER NOT NULL DEFAULT 0   -- bumps when video re-encoded
);

-- Per-contributor account, keyed by Ed25519 pubkey hex.
CREATE TABLE IF NOT EXISTS account (
    pub          TEXT PRIMARY KEY,
    credits      INTEGER NOT NULL DEFAULT 0,   -- spendable
    reputation   REAL NOT NULL DEFAULT 0.0,    -- grows with accepted work
    tiles        INTEGER NOT NULL DEFAULT 0,   -- lifetime accepted tiles
    tile_remainder INTEGER NOT NULL DEFAULT 0, -- accepted tiles not yet a credit
    backings_used INTEGER NOT NULL DEFAULT 0,  -- votes cast this lifetime
    banned       INTEGER NOT NULL DEFAULT 0,   -- 1 = fraud caught, all work void
    last_nonce   INTEGER NOT NULL DEFAULT 0,   -- replay guard (monotonic)
    created_ms   INTEGER NOT NULL DEFAULT 0
);

-- Assignment ledger: every (sheep, frame, idx) tile that has been handed out.
-- The (sheep_id, frame, idx) primary key is the collision guard — a tile is
-- assigned to exactly one pubkey, never re-handed while pending.
--   status: 0 = assigned (pending), 1 = accepted (merged), 2 = rejected/free.
CREATE TABLE IF NOT EXISTS tile (
    sheep_id    TEXT NOT NULL,
    frame       INTEGER NOT NULL,
    idx         INTEGER NOT NULL,
    pub         TEXT,                   -- who it's assigned to (NULL once free)
    status      INTEGER NOT NULL DEFAULT 0,
    assigned_ms INTEGER NOT NULL,
    hash        TEXT,                   -- verified content hash once accepted
    PRIMARY KEY (sheep_id, frame, idx)
);
CREATE INDEX IF NOT EXISTS tile_by_status ON tile(status);
CREATE INDEX IF NOT EXISTS tile_by_pub ON tile(pub);

-- Votes this generation: one row per (gen, pub, sheep). Enforces a spent credit.
CREATE TABLE IF NOT EXISTS vote (
    gen         INTEGER NOT NULL,
    pub         TEXT NOT NULL,
    sheep_id    TEXT NOT NULL,
    ts_ms       INTEGER NOT NULL,
    PRIMARY KEY (gen, pub, sheep_id)
);
CREATE INDEX IF NOT EXISTS vote_by_sheep ON vote(gen, sheep_id);

-- Accepted tiles per sheep, ordered for replay (the canonical reconstruct log).
CREATE INDEX IF NOT EXISTS tile_accepted_by_sheep ON tile(sheep_id, status);

-- Coverage cache: how many idxs of each (sheep, frame) have been accepted, so
-- /api/sheep can report frames_coverage without scanning the tile table hot.
CREATE TABLE IF NOT EXISTS coverage (
    sheep_id    TEXT NOT NULL,
    frame       INTEGER NOT NULL,
    accepted    INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (sheep_id, frame)
);

-- Peer-audit ledger: an outstanding audit task handed to an auditor by /assign,
-- awaiting their re-rendered hash report in a later /submit. This is what lets
-- the coordinator OFFLOAD verification — it plants the task and grades the
-- reply instead of re-rendering itself.
--
--   auditor       the pubkey we handed this task to (who must report a hash)
--   sheep_id/frame/idx   the tile to re-render (a real submitter's tile, or a
--                        honeypot the coordinator authored)
--   kind          0 = real audit (grade against the submitter's stored hash)
--                 1 = honeypot   (grade against `expected_hash`, known up front)
--   expected_hash for honeypots: the hash the coordinator already computed at
--                 plant time (free grading, no re-render on report). NULL for
--                 real audits (their truth is the tile's stored content hash).
--   submitter     for real audits: the tile's submitter pubkey, captured at
--                 plant time so a dispute can identify the accused. NULL for
--                 honeypots (no real submitter — the coordinator authored it).
--   status        0 = pending (handed out, awaiting report)
--                 1 = resolved (report received + graded)
--   created_ms    when planted (so stale/unreported audits can be reaped).
CREATE TABLE IF NOT EXISTS audit (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    auditor       TEXT NOT NULL,
    sheep_id      TEXT NOT NULL,
    frame         INTEGER NOT NULL,
    idx           INTEGER NOT NULL,
    kind          INTEGER NOT NULL DEFAULT 0,
    expected_hash TEXT,
    submitter     TEXT,
    status        INTEGER NOT NULL DEFAULT 0,
    created_ms    INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS audit_by_auditor ON audit(auditor, status);
CREATE INDEX IF NOT EXISTS audit_open_tile ON audit(sheep_id, frame, idx, status);
"#;
