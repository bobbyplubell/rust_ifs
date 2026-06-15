//! Disk safety: keep accumulated histograms a *bounded working cache*, never an
//! unbounded store that can fill the host.
//!
//! THE PROBLEM this module owns: each sheep's accumulated histogram is
//! ~576 MB on disk (`<DATA_DIR>/hist/<id>/frame_NNNN.bin`), fixed by resolution.
//! On a 30 GB droplet ~50 sheep would fill the box, corrupting SQLite writes and
//! crashing the host. So histograms are demoted to a regenerable cache with two
//! guardrails and a degradation path:
//!
//! 1. **Free-space floor** (hard backstop) — before any hist write we check the
//!    *actual* filesystem free space (`fs2::available_space`). If a write would
//!    push free space below `FREE_FLOOR`, we don't write. The box never hits 0.
//! 2. **Hist-disk cap + LRU eviction** — total bytes under `hist/` are kept
//!    under `HIST_CAP`. When a merge would exceed the cap (or threaten the
//!    floor), we evict the **least-recently-merged** sheep: delete its
//!    `hist/<id>/` files but KEEP its video + its tile ledger in SQLite.
//! 3. **Reconstruct-from-log on reactivation** — eviction is lossless because
//!    the tile ledger (`tile` rows with status=accepted) records every accepted
//!    `(sheep,frame,idx)`. If an evicted sheep gets new accepted tiles we
//!    re-render its accepted tiles from the log before merging the new ones.
//! 4. **Graceful degradation** — if eviction can't free enough (the live set
//!    alone exceeds the cap), the caller still accepts + records the tile in the
//!    ledger (so credit + the collision guard hold), but the hist merge is
//!    SKIPPED with a WARNING. The histogram is reconstructable later from the
//!    log; nothing is lost, the disk never fills, and we never 500.
//!
//! The LRU key is `sheep.hist_touched_ms` (unix-ms of the last merge into that
//! sheep). `sheep.hist_bytes` caches each sheep's on-disk hist size so we can
//! total usage and pick eviction victims without statting the tree every time.

use std::path::{Path, PathBuf};

use flame_core::chunked;
use flame_core::genome::Genome;
use flame_core::render::Accum;

use crate::db::Db;
use crate::ga::now_ms;
use crate::render;
use crate::spec;

/// Default free-space floor: never let the filesystem drop below ~2 GB free.
pub const FREE_FLOOR_DEFAULT: u64 = 2 * 1024 * 1024 * 1024;
/// Default histogram-disk cap: keep total `hist/` bytes under ~15 GB.
pub const HIST_CAP_DEFAULT: u64 = 15 * 1024 * 1024 * 1024;

/// Bytes one sheep's full histogram occupies on disk (all frames), so the
/// floor/cap math can reason about a write *before* it happens. Fixed by spec:
/// `W*SS * H*SS * 4ch * 8B * N_FRAMES`.
pub fn sheep_hist_size() -> u64 {
    spec::hist_cells() as u64 * 8 * spec::N_FRAMES as u64
}

/// Tunables, read once from the environment with 30 GB-droplet defaults.
#[derive(Clone, Copy, Debug)]
pub struct DiskConfig {
    /// Never write a histogram if doing so would leave less than this free.
    pub free_floor: u64,
    /// Keep total bytes under `hist/` at or below this.
    pub hist_cap: u64,
}

impl DiskConfig {
    pub fn from_env() -> Self {
        let free_floor = env_bytes("FREE_FLOOR", FREE_FLOOR_DEFAULT);
        let hist_cap = env_bytes("HIST_CAP", HIST_CAP_DEFAULT);
        tracing::info!(
            "disk guard: FREE_FLOOR={} MB, HIST_CAP={} MB, per-sheep hist ~{} MB",
            free_floor / (1024 * 1024),
            hist_cap / (1024 * 1024),
            sheep_hist_size() / (1024 * 1024),
        );
        DiskConfig { free_floor, hist_cap }
    }
}

fn env_bytes(key: &str, default: u64) -> u64 {
    std::env::var(key).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(default)
}

/// Root of the histogram cache: `<data_dir>/hist`.
pub fn hist_root(data_dir: &Path) -> PathBuf {
    data_dir.join("hist")
}

/// Per-sheep histogram directory: `<data_dir>/hist/<sheep_id>`.
fn sheep_hist_dir(data_dir: &Path, sheep_id: &str) -> PathBuf {
    hist_root(data_dir).join(sheep_id)
}

/// Actual filesystem free space at `path` (the hard backstop). On error we
/// return 0 — i.e. "assume full" — so a probe failure fails *safe* (no write).
pub fn free_bytes(path: &Path) -> u64 {
    // statvfs needs an existing path; walk up to the nearest ancestor.
    let mut p = path;
    loop {
        match fs2::available_space(p) {
            Ok(b) => return b,
            Err(_) => match p.parent() {
                Some(parent) => p = parent,
                None => return 0,
            },
        }
    }
}

/// Sum the on-disk byte size of one sheep's `hist/<id>/` dir (stat the files).
fn dir_bytes(dir: &Path) -> u64 {
    let mut total = 0u64;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            if let Ok(m) = e.metadata() {
                total += m.len();
            }
        }
    }
    total
}

/// Total tracked histogram bytes (sum of `sheep.hist_bytes`). Cheap — no stat.
pub fn tracked_hist_bytes(db: &Db) -> u64 {
    let conn = db.conn.lock().unwrap();
    conn.query_row("SELECT COALESCE(SUM(hist_bytes), 0) FROM sheep", [], |r| r.get::<_, i64>(0))
        .map(|v| v.max(0) as u64)
        .unwrap_or(0)
}

/// Refresh `sheep.hist_bytes` / `hist_touched_ms` from the on-disk dir after a
/// merge or reconstruct. `touched` bumps the LRU key.
fn note_hist(db: &Db, data_dir: &Path, sheep_id: &str, touched: bool) {
    let bytes = dir_bytes(&sheep_hist_dir(data_dir, sheep_id)) as i64;
    let conn = db.conn.lock().unwrap();
    if touched {
        let _ = conn.execute(
            "UPDATE sheep SET hist_bytes = ?1, hist_touched_ms = ?2 WHERE id = ?3",
            rusqlite::params![bytes, now_ms() as i64, sheep_id],
        );
    } else {
        let _ = conn.execute(
            "UPDATE sheep SET hist_bytes = ?1 WHERE id = ?2",
            rusqlite::params![bytes, sheep_id],
        );
    }
}

/// Does this sheep currently have any histogram files on disk?
fn has_hist_on_disk(data_dir: &Path, sheep_id: &str) -> bool {
    std::fs::read_dir(sheep_hist_dir(data_dir, sheep_id))
        .map(|mut rd| rd.next().is_some())
        .unwrap_or(false)
}

/// Delete one sheep's `hist/<id>/` dir, zero its tracked bytes. Returns freed
/// bytes. The video + tile ledger are untouched (eviction is lossless).
fn evict_sheep(db: &Db, data_dir: &Path, sheep_id: &str) -> u64 {
    let dir = sheep_hist_dir(data_dir, sheep_id);
    let freed = dir_bytes(&dir);
    let _ = std::fs::remove_dir_all(&dir);
    {
        let conn = db.conn.lock().unwrap();
        // hist_bytes -> 0; hist_touched_ms -> 0 marks "evicted / no hist".
        let _ = conn.execute(
            "UPDATE sheep SET hist_bytes = 0, hist_touched_ms = 0 WHERE id = ?1",
            [sheep_id],
        );
    }
    tracing::warn!(
        "disk: evicted histogram for sheep {} (freed {} MB); video + tile log preserved",
        &sheep_id[..sheep_id.len().min(12)],
        freed / (1024 * 1024),
    );
    freed
}

/// Pick the least-recently-merged sheep that still has hist on disk, excluding
/// `keep` (the sheep we're about to write). Returns its id, or None if there is
/// nothing left to evict.
fn lru_victim(db: &Db, keep: &str) -> Option<String> {
    let conn = db.conn.lock().unwrap();
    conn.query_row(
        "SELECT id FROM sheep
         WHERE hist_bytes > 0 AND id != ?1
         ORDER BY hist_touched_ms ASC, hist_bytes DESC
         LIMIT 1",
        [keep],
        |r| r.get::<_, String>(0),
    )
    .ok()
}

/// Outcome of trying to make room for a sheep's hist write.
#[derive(Debug, PartialEq, Eq)]
enum Room {
    /// There is room (after any evictions performed) — proceed with the merge.
    Ok,
    /// Could not free enough without evicting the target itself — caller must
    /// degrade gracefully (accept the tile, skip the merge).
    Degrade,
}

/// Ensure there's room to (re)write `sheep_id`'s histogram: evict LRU sheep
/// until total tracked hist + headroom fits under the cap AND the write won't
/// breach the free-space floor. `incoming` is the extra bytes this write may add
/// on top of what `sheep_id` already has on disk (0 if it's already present).
fn make_room(cfg: &DiskConfig, db: &Db, data_dir: &Path, sheep_id: &str, incoming: u64) -> Room {
    loop {
        let tracked = tracked_hist_bytes(db);
        let free = free_bytes(data_dir);

        let over_cap = tracked.saturating_add(incoming) > cfg.hist_cap;
        // Floor check against ACTUAL free space: would the write drop us below
        // the floor? (fs2 measures real disk, catching anything else eating it.)
        let under_floor = free.saturating_sub(incoming) < cfg.free_floor;

        if !over_cap && !under_floor {
            return Room::Ok;
        }
        if over_cap {
            tracing::warn!(
                "disk: hist usage {} MB + {} MB incoming exceeds cap {} MB — evicting LRU",
                tracked / (1024 * 1024),
                incoming / (1024 * 1024),
                cfg.hist_cap / (1024 * 1024),
            );
        }
        if under_floor {
            tracing::warn!(
                "disk: free {} MB would drop below floor {} MB — evicting LRU",
                free / (1024 * 1024),
                cfg.free_floor / (1024 * 1024),
            );
        }

        match lru_victim(db, sheep_id) {
            Some(victim) => {
                evict_sheep(db, data_dir, &victim);
            }
            None => {
                // Nothing left to evict but the target itself. Degrade.
                tracing::warn!(
                    "disk: cannot free enough for sheep {} (live set exceeds cap / floor) — \
                     accepting tile + recording in ledger but SKIPPING hist merge \
                     (reconstructable from log later)",
                    &sheep_id[..sheep_id.len().min(12)],
                );
                return Room::Degrade;
            }
        }
    }
}

/// Reconstruct an evicted sheep's histogram from the canonical tile ledger:
/// re-render every accepted `(frame, idx)` and sum into the per-frame accum,
/// then persist. This is what makes eviction safe — the log is canonical, the
/// histogram is regenerable. No-op if the sheep already has hist on disk.
///
/// Honors the floor/cap exactly like a merge: if there's no room, we degrade
/// (leave the sheep without a reconstructed hist) rather than fill the disk.
pub fn reconstruct_if_evicted(
    cfg: &DiskConfig,
    db: &Db,
    data_dir: &Path,
    sheep_id: &str,
    genome: &Genome,
) -> ApiResultUnit {
    if has_hist_on_disk(data_dir, sheep_id) {
        return Ok(());
    }
    // Pull the accepted (frame, idx) ledger for this sheep.
    let accepted: Vec<(u32, u32)> = {
        let conn = db.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT frame, idx FROM tile WHERE sheep_id = ?1 AND status = 1 ORDER BY frame, idx",
            )
            .map_err(|_| ())?;
        let rows = stmt
            .query_map([sheep_id], |r| {
                Ok((r.get::<_, i64>(0)? as u32, r.get::<_, i64>(1)? as u32))
            })
            .map_err(|_| ())?;
        rows.filter_map(Result::ok).collect()
    };
    if accepted.is_empty() {
        return Ok(()); // nothing to reconstruct (fresh sheep)
    }

    // Room for a full reconstruct (worst case: the whole sheep hist).
    if make_room(cfg, db, data_dir, sheep_id, sheep_hist_size()) == Room::Degrade {
        tracing::warn!(
            "disk: skipping reconstruct for sheep {} (no room) — new tiles this round \
             merge onto a partial/empty hist; full reconstruct deferred",
            &sheep_id[..sheep_id.len().min(12)],
        );
        return Ok(());
    }

    let sid_bytes = render::sheep_id_bytes(sheep_id).map_err(|_| ())?;
    tracing::info!(
        "disk: reconstructing histogram for sheep {} from {} accepted tiles",
        &sheep_id[..sheep_id.len().min(12)],
        accepted.len(),
    );

    // Group by frame; render+sum each frame's accepted idxs into one accum.
    let (w, h, ss, nf) = (spec::W, spec::H, spec::SS, spec::N_FRAMES);
    let mut cur_frame: Option<u32> = None;
    let mut accum: Option<Accum> = None;
    let flush = |db: &Db, frame: u32, accum: &Accum| {
        if let Err(e) = render::save_frame_accum(data_dir, sheep_id, frame, accum) {
            tracing::warn!("disk: reconstruct save frame {frame} failed: {}", e.msg);
        }
        note_hist(db, data_dir, sheep_id, false);
    };
    for (frame, idx) in accepted {
        if cur_frame != Some(frame) {
            if let (Some(f), Some(a)) = (cur_frame, accum.as_ref()) {
                flush(db, f, a);
            }
            cur_frame = Some(frame);
            accum = Some(Accum::new((w * ss) as usize, (h * ss) as usize));
        }
        let part = chunked::render_batch(
            genome, &sid_bytes, frame, idx,
            w as usize, h as usize, ss as usize, spec::SPP, nf,
        );
        if let Some(a) = accum.as_mut() {
            for (cell, src) in a.data.iter_mut().zip(part.data.iter()) {
                cell[0] = cell[0].saturating_add(src[0]);
                cell[1] = cell[1].saturating_add(src[1]);
                cell[2] = cell[2].saturating_add(src[2]);
                cell[3] = cell[3].saturating_add(src[3]);
            }
        }
    }
    if let (Some(f), Some(a)) = (cur_frame, accum.as_ref()) {
        flush(db, f, a);
    }
    // Stamp the LRU key now that the hist is whole again.
    note_hist(db, data_dir, sheep_id, true);
    Ok(())
}

/// Gated merge: the single entry point the submit path uses instead of calling
/// `render::merge_tile_into_frame` directly.
///
/// Steps: (1) if the sheep was evicted, reconstruct it from the log first so the
/// new tile sums onto complete state; (2) make room under the cap/floor by
/// evicting LRU sheep; (3) if room exists, merge + bump the LRU key; otherwise
/// DEGRADE — skip the merge, returning `merged=false` so the caller still
/// accepts the tile and records it in the ledger (reconstructable later).
///
/// Returns `Ok(true)` if merged, `Ok(false)` if degraded (never an error for a
/// disk-full condition — we never 500 / crash / fill the disk).
#[allow(clippy::too_many_arguments)]
pub fn merge_tile(
    cfg: &DiskConfig,
    db: &Db,
    data_dir: &Path,
    sheep_id: &str,
    genome: &Genome,
    frame: u32,
    contribution: &[u64],
) -> ApiResultBool {
    // (1) Reconstruct-from-log if this sheep's hist was evicted. Lossless: the
    // new tile then merges onto its full accumulated state, not a blank.
    let _ = reconstruct_if_evicted(cfg, db, data_dir, sheep_id, genome);

    // (2) Room for this frame's incremental write. A single frame is small; the
    // first write for an absent sheep may grow the dir by a frame, but to keep
    // the cap honest against a sheep that's about to fill out we reserve a whole
    // sheep's worth of headroom only when the sheep has nothing on disk yet.
    let incoming = if has_hist_on_disk(data_dir, sheep_id) {
        // Merge rewrites one frame file in place; net growth ~0.
        0
    } else {
        // First touch — this sheep will accumulate toward a full hist.
        sheep_hist_size()
    };
    if make_room(cfg, db, data_dir, sheep_id, incoming) == Room::Degrade {
        return Ok(false); // graceful degradation: caller still accepts the tile
    }

    // (3) Merge into the per-frame accumulated histogram on disk.
    render::merge_tile_into_frame(
        data_dir, sheep_id, frame, contribution, spec::W, spec::H, spec::SS,
    )
    .map_err(|e| {
        tracing::warn!("disk: merge for sheep {sheep_id} frame {frame} failed: {}", e.msg);
        ()
    })?;
    note_hist(db, data_dir, sheep_id, true);
    Ok(true)
}

/// Drop a sheep's histogram cache so it is rebuilt from the (now-cleaned) tile
/// log on the next merge/repaint. Used by the dispute path to SUBTRACT a banned
/// submitter's fraudulent contribution: their tiles are already removed from the
/// log (status != accepted), so the reconstruct contains only honest pixels.
/// Lossless — the video + tile ledger are untouched (same as LRU eviction).
pub fn evict_for_subtract(_cfg: &DiskConfig, db: &Db, data_dir: &Path, sheep_id: &str) {
    if has_hist_on_disk(data_dir, sheep_id) {
        evict_sheep(db, data_dir, sheep_id);
    }
}

/// Observability snapshot for `/health` / `/api/stats`.
pub fn stats(cfg: &DiskConfig, db: &Db, data_dir: &Path) -> serde_json::Value {
    let hist = tracked_hist_bytes(db);
    let free = free_bytes(data_dir);
    serde_json::json!({
        "hist_bytes": hist,
        "hist_cap_bytes": cfg.hist_cap,
        "free_bytes": free,
        "free_floor_bytes": cfg.free_floor,
        "per_sheep_hist_bytes": sheep_hist_size(),
        "hist_over_cap": hist > cfg.hist_cap,
        "free_under_floor": free < cfg.free_floor,
    })
}

// Local result aliases: a disk-full condition is never a 500. We signal "could
// not write" with a unit error that the caller maps to graceful degradation,
// not an HTTP error.
type ApiResultUnit = Result<(), ()>;
type ApiResultBool = Result<bool, ()>;

#[cfg(test)]
mod tests {
    use super::*;
    use flame_core::canonical::{sheep_id, sheep_id_hex};
    use flame_core::genome::Genome;
    use flame_core::rng::Rng;

    /// A throwaway data dir + DB that cleans up on drop.
    struct Fixture {
        dir: PathBuf,
        db: Db,
    }
    impl Fixture {
        fn new(tag: &str) -> Self {
            let mut dir = std::env::temp_dir();
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            dir.push(format!("disktest-{tag}-{nanos}"));
            std::fs::create_dir_all(&dir).unwrap();
            let db = Db::open(dir.join("c.sqlite").to_str().unwrap()).unwrap();
            Fixture { dir, db }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Insert a sheep row + log `frames`×`idxs_per_frame` accepted tiles for it.
    /// Returns (sheep_id_hex, genome). Tiles are the canonical reconstruct log.
    fn seed_sheep(fx: &Fixture, seed: u64, frames: u32, idxs: u32) -> (String, Genome) {
        let mut rng = Rng::new(seed);
        let genome = Genome::random(&mut rng, 3);
        let id = sheep_id_hex(&genome);
        let json = flame_core::canonical::canonical_json(&genome);
        let conn = fx.db.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO meta (id, gen, gen_started_ms, gen_ms) VALUES (0,0,0,0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sheep
             (id,name,genome,gen,n_frames,w,h,ss,spp,alive,hof,tiles,created_ms,video_rev)
             VALUES (?1,'t',?2,0,?3,?4,?5,?6,?7,1,0,0,0,0)",
            rusqlite::params![id, json, spec::N_FRAMES, spec::W, spec::H, spec::SS, spec::SPP],
        )
        .unwrap();
        for frame in 0..frames {
            for idx in 0..idxs {
                conn.execute(
                    "INSERT INTO tile (sheep_id,frame,idx,pub,status,assigned_ms,hash)
                     VALUES (?1,?2,?3,'k',1,0,'h')",
                    rusqlite::params![id, frame, idx],
                )
                .unwrap();
            }
        }
        (id, genome)
    }

    fn zero_contribution() -> Vec<u64> {
        vec![0u64; spec::hist_cells()]
    }

    /// Eviction: a tiny cap forces the LRU sheep's hist to be deleted when a
    /// second sheep is written — but its tile log (and a stand-in video) remain,
    /// and reactivating it reconstructs the hist from the log.
    #[test]
    fn eviction_floor_and_reconstruct() {
        let fx = Fixture::new("evict");
        // Cap below two sheep's first-touch reservation so the 2nd write evicts
        // the 1st. Floor 0 so the cap (not the FS floor) drives this test.
        // Exactly one sheep's worth: a single fresh sheep's first-touch
        // reservation just fits, but a second one's does not (its reservation
        // plus the first's real on-disk bytes exceeds the cap), forcing the LRU
        // sheep to be evicted.
        let cfg = DiskConfig { free_floor: 0, hist_cap: sheep_hist_size() };

        // Two sheep, each with a couple accepted tiles logged. Keep it to a
        // single frame so reconstruct renders only a few tiles.
        let (a, ga) = seed_sheep(&fx, 11, 1, 2);
        let (b, gb) = seed_sheep(&fx, 22, 1, 2);

        // Merge sheep A first (becomes LRU), then sheep B.
        let m_a = merge_tile(&cfg, &fx.db, &fx.dir, &a, &ga, 0, &zero_contribution()).unwrap();
        assert!(m_a, "first merge should land");
        assert!(has_hist_on_disk(&fx.dir, &a), "A has hist after merge");

        // Pretend A also has a video on disk; eviction must NOT touch it.
        let vdir = fx.dir.join("video");
        std::fs::create_dir_all(&vdir).unwrap();
        let a_video = vdir.join(format!("{a}.webm"));
        std::fs::write(&a_video, b"VIDEO").unwrap();

        // Make B's touch newer so A is unambiguously the LRU victim.
        std::thread::sleep(std::time::Duration::from_millis(5));
        let m_b = merge_tile(&cfg, &fx.db, &fx.dir, &b, &gb, 0, &zero_contribution()).unwrap();
        assert!(m_b, "second merge should land (after evicting A)");

        // A's hist must be GONE (evicted); B's must be present.
        assert!(!has_hist_on_disk(&fx.dir, &a), "A hist evicted");
        assert!(has_hist_on_disk(&fx.dir, &b), "B hist present");

        // A's video + tile ledger must SURVIVE eviction.
        assert!(a_video.exists(), "A video preserved across eviction");
        let a_tiles: i64 = {
            let conn = fx.db.conn.lock().unwrap();
            conn.query_row(
                "SELECT COUNT(*) FROM tile WHERE sheep_id = ?1 AND status = 1",
                [&a],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(a_tiles, 2, "A tile log preserved across eviction");
        // Eviction zeroed A's tracked bytes.
        let a_bytes: i64 = {
            let conn = fx.db.conn.lock().unwrap();
            conn.query_row("SELECT hist_bytes FROM sheep WHERE id = ?1", [&a], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(a_bytes, 0, "evicted sheep tracks 0 hist bytes");

        // Reactivate A with a NEW accepted tile. Raise the cap so room exists for
        // both, exercising the reconstruct-from-log path before the new merge.
        let cfg2 = DiskConfig { free_floor: 0, hist_cap: sheep_hist_size() * 4 };
        let m_a2 = merge_tile(&cfg2, &fx.db, &fx.dir, &a, &ga, 0, &zero_contribution()).unwrap();
        assert!(m_a2, "reactivation merge lands");
        assert!(has_hist_on_disk(&fx.dir, &a), "A hist reconstructed from log");

        // The reconstructed frame must carry real counts (the log re-rendered),
        // not be an empty file — prove reconstruct actually rendered the tiles.
        let accum = render::load_frame_accum(&fx.dir, &a, 0, spec::W, spec::H, spec::SS);
        let total: u64 = accum.data.iter().map(|c| c[3]).sum();
        assert!(total > 0, "reconstructed hist has nonzero counts from re-render");

        // Sanity: the rendered hash for an accepted tile is reproducible (the log
        // is canonical) — re-render idx 0 and confirm it hashes deterministically.
        let sid = sheep_id(&ga);
        let h1 = render::verify_tile_hash(
            &ga, &sid, 0, 0, spec::W, spec::H, spec::SS, spec::SPP, spec::N_FRAMES,
        );
        let h2 = render::verify_tile_hash(
            &ga, &sid, 0, 0, spec::W, spec::H, spec::SS, spec::SPP, spec::N_FRAMES,
        );
        assert_eq!(h1, h2, "log replay is deterministic");
    }

    /// Graceful degradation: when the cap is smaller than a single sheep's
    /// first-touch reservation and there's nothing else to evict, the merge is
    /// SKIPPED (returns false) rather than writing — but never errors.
    #[test]
    fn degrades_when_live_set_exceeds_cap() {
        let fx = Fixture::new("degrade");
        // Cap below one sheep — and it's the only sheep, so nothing to evict.
        let cfg = DiskConfig { free_floor: 0, hist_cap: sheep_hist_size() / 2 };
        let (a, ga) = seed_sheep(&fx, 33, 1, 1);

        let merged = merge_tile(&cfg, &fx.db, &fx.dir, &a, &ga, 0, &zero_contribution()).unwrap();
        assert!(!merged, "merge degrades (skips) when it can't fit under cap");
        assert!(!has_hist_on_disk(&fx.dir, &a), "no hist written on degrade");
        // But this is NOT an error — the caller accepts the tile regardless.
    }
}
