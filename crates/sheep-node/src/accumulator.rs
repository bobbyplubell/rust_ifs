//! The **accumulate** capability (ARCHITECTURE v3 §1.1, §5) — the heavy-data
//! layer, a struct SEPARATE from the pure [`crate::engine::Engine`].
//!
//! A worker renders tiles and uploads each as a [`PieceUpload`] (a compressed
//! tile histogram, §5). An [`Accumulator`] ingests those pieces and maintains,
//! per `(sheep, frame)`, the **element-wise integer sum** of all that frame's
//! `(idx, pass)` tile histograms — the dense, progressively-resolving frame
//! image that video / display consume (`tonemap`).
//!
//! ## Memory model: merged-only + disk-backed LRU
//!
//! A single full-frame R384 tile is ~4.7 MB (384²×32 B). Keeping every raw tile
//! for keyed retraction would cost ~38 GB per fully-rendered sheep → OOM. So this
//! accumulator is **merged-only**: per `(sheep, frame)` it keeps ONLY
//!
//! - the running element-wise sum [`Accum`] (each ingested tile is folded in and
//!   then *discarded* — the raw bytes are not retained),
//! - a [`HashSet`] of the **content-hashes already folded** (idempotency: a hash
//!   already folded is a no-op), and
//! - a running `u64` sample count (each folded tile's `total_count` added in, so
//!   [`Accumulator::total_count`] needs no merged-buffer load).
//!
//! The heavy merged `Accum` buffers are held in a **bounded RAM LRU** keyed by
//! `(sheep, frame)`; the small per-frame metadata (folded-hash set, count, dims)
//! is ALWAYS resident (a separate map, never evicted). When the resident merged
//! set would exceed the frame budget derived from `ram_budget_mb`, the
//! least-recently-used frame is flushed to disk (`encode_accum` bytes under
//! `data_dir/accum/`) if dirty, then dropped from RAM; a later access reloads it
//! via `decode_accum`. The spill dir is a within-run cache — the CRDT
//! re-accumulates from gossip after a restart — so it is cleared on construction.
//!
//! ## Why this is a CRDT (§1.1)
//!
//! flame-core histograms are **content-addressed integer buffers** whose merge
//! (`Accum::merge`) is element-wise `u64` addition — commutative and
//! associative. The accumulator folds each tile by its **content hash**, so:
//!
//! - **commutative + associative**: the merged frame is a sum, independent of
//!   ingest order;
//! - **idempotent**: re-ingesting a piece whose hash is already folded is a no-op
//!   (the same content can't be double-counted), and a hash in the **removed-set**
//!   stays barred from re-entry;
//! - **convergent**: any set of accumulators that ingest the same confirmed-piece
//!   set reach byte-identical merged state with zero coordination.
//!
//! Fraud retraction (§6) has a **dual API**, because the §6 dispute path produces
//! HASHES only (the fraudulent bytes aren't available there) while a test/caller
//! that holds the bytes can subtract exactly:
//!
//! - [`Accumulator::retract_hash`] marks a hash removed (the always-correct
//!   guarantee: it can never re-enter the merge). Because we DISCARD raw tiles, an
//!   already-folded fraudulent tile cannot be exactly subtracted here — exact
//!   subtraction of an already-folded tile is deferred to the Phase-2 pull-based
//!   frame rebuild (re-pull the frame's live pieces and re-sum). For now the
//!   removed-set prevents re-entry, and affected frames are flagged for rebuild.
//! - [`Accumulator::retract`] subtracts a **provided** `Accum` (the caller has the
//!   bytes) from every frame that folded its hash, exactly — and marks it removed.
//!
//! ## Trust
//!
//! Ingest verifies `piece.hash == hist_hash_hex(decoded)` and rejects a mismatch
//! with **no merge** — the accumulator never trusts a claimed hash, it hashes the
//! bytes. (Peer/gateway audit, §6, is the engine's job; the accumulator's own gate
//! is content-integrity.)

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use flame_core::chunked::{hist_hash_hex, subtract_ok, total_count};
use flame_core::genome::Genome;
use flame_core::render::{tonemap, Accum};

use crate::hist::{decode_accum, encode_accum};

/// Key for one frame's accumulation: `(sheep_id hex, frame)`.
type FrameKey = (String, u32);

/// Always-resident per-frame metadata. Small (a hash set + two integers + dims),
/// so it is NEVER evicted — only the heavy merged [`Accum`] buffer spills to disk.
struct FrameMeta {
    /// Content-hashes already folded into the merged sum (the CRDT's content-
    /// addressing + idempotency: a hash present here is a no-op to re-ingest, and
    /// identifies which frames a retracted hash poisoned).
    folded: HashSet<String>,
    /// Running sample count: sum of every folded tile's `total_count`. Lets
    /// [`Accumulator::total_count`] answer without loading any merged buffer.
    count: u64,
    /// Histogram dimensions (edge x edge) for this sheep's tiles — needed to
    /// allocate / decode the merged buffer on a reload.
    dims: (usize, usize),
    /// Frames flagged for a Phase-2 rebuild because a hash they folded was
    /// retracted via [`Accumulator::retract_hash`] (no bytes to subtract). Carried
    /// for diagnostics / the future pull-based rebuild; the removed-set already
    /// guarantees the hash can't re-enter.
    needs_rebuild: bool,
}

/// One resident merged frame buffer + its dirty flag (LRU entry). Only this — the
/// heavy `w*h*32`-byte buffer — is bounded and spillable.
struct Resident {
    merged: Accum,
    /// Modified since the last flush → must be flushed before eviction.
    dirty: bool,
}

/// The accumulate-capability node role (§1.1). Holds one merged per-`(sheep,
/// frame)` CRDT sum (RAM-bounded, disk-backed) and the tonemap hook video/display
/// read.
pub struct Accumulator {
    /// Always-resident small per-frame metadata (folded-hash set + count + dims).
    meta: HashMap<FrameKey, FrameMeta>,
    /// Bounded RAM LRU of heavy merged buffers. `order` is the LRU recency list
    /// (front = most-recently-used); a frame absent here lives on disk only.
    resident: HashMap<FrameKey, Resident>,
    /// Recency order for `resident` (front = most-recently-used, back = LRU). A
    /// `VecDeque`-free `Vec` is fine: the resident set is tiny (a handful of
    /// frames at typical budgets), so the linear touch/evict is cheap.
    order: Vec<FrameKey>,
    /// Max merged frames resident at once, derived from the RAM budget and the
    /// frame size on first ingest. `0` until the first frame fixes the dims (then
    /// `max(1, budget_bytes / frame_bytes)`).
    max_frames: usize,
    /// RAM budget in megabytes (MB = 1_000_000 bytes), fixed at construction.
    ram_budget_mb: usize,
    /// Spill directory root (`<data_dir>/accum`). Each frame flushes to
    /// `<spill>/<sheep_hex>/<frame>.acc`.
    spill_dir: PathBuf,
    /// Sheep genomes (needed only for `tonemap`'s palette/gamma). Populated via
    /// [`Accumulator::register_sheep`].
    genomes: HashMap<String, Genome>,
    /// Content hashes retracted as fraud (§6). A tile whose hash is in here is
    /// barred from ingest forever; re-ingesting it stays a no-op.
    removed: HashSet<String>,
}

impl Accumulator {
    /// Construct an accumulator that spills merged frames under `data_dir/accum/`
    /// and holds a RAM working set bounded by `ram_budget_mb` (MB = 1e6 bytes).
    ///
    /// The spill dir is a **within-run** cache: the CRDT re-accumulates from
    /// gossip after a restart, so any orphaned spill from a previous run is
    /// CLEARED here (a stale `<frame>.acc` could otherwise be reloaded as though
    /// it were this run's sum). A failure to clear/create the dir is non-fatal —
    /// flush/reload then degrade to no-ops (the merged set just stays RAM-only).
    pub fn new(data_dir: PathBuf, ram_budget_mb: usize) -> Self {
        let spill_dir = data_dir.join("accum");
        // Clear any prior-run spill, then (re)create the empty dir.
        let _ = std::fs::remove_dir_all(&spill_dir);
        let _ = std::fs::create_dir_all(&spill_dir);
        Accumulator {
            meta: HashMap::new(),
            resident: HashMap::new(),
            order: Vec::new(),
            max_frames: 0,
            ram_budget_mb,
            spill_dir,
            genomes: HashMap::new(),
            removed: HashSet::new(),
        }
    }

    /// Register a sheep's genome so [`Accumulator::tonemap`] can resolve its
    /// palette / gamma / background. (Ingest itself needs no genome — it only
    /// sums histograms — so an accumulator can ingest before learning the
    /// genome; tonemap just needs it by display time.)
    pub fn register_sheep(&mut self, sheep_id: &str, genome: Genome) {
        self.genomes.insert(sheep_id.to_string(), genome);
    }

    /// Ingest one [`PieceUpload`]. Returns `true` if it changed state, `false`
    /// if rejected (hash mismatch / undecodable / wrong size) or a no-op
    /// (already-folded content-hash, or a retracted hash).
    ///
    /// `edge` is the tile's pixel edge (`resolution.edge()`), needed to decode
    /// the flat histogram back into a 2-D `Accum`. The decoded tile is folded into
    /// the frame's merged sum and then DISCARDED (merged-only model).
    pub fn ingest(&mut self, piece: &sheep_proto::msg::PieceUpload, edge: usize) -> bool {
        // Retracted content never re-enters (idempotent under fraud removal).
        if self.removed.contains(&piece.hash) {
            return false;
        }

        let key: FrameKey = (piece.sheep_id.clone(), piece.frame);

        // Idempotent by content-hash: re-folding the same tile is a no-op. The
        // folded set is always resident, so this is a cheap pre-decode gate.
        if let Some(m) = self.meta.get(&key) {
            if m.folded.contains(&piece.hash) {
                return false;
            }
        }

        // Decode the heavy artifact, then VERIFY content-integrity: the stored
        // hash must equal the hash of the bytes. No merge, no trust of the
        // claimed hash.
        let Some(tile) = decode_accum(&piece.hist_b64, edge, edge) else {
            return false;
        };
        if hist_hash_hex(&tile) != piece.hash {
            return false;
        }

        // Fold into the resident merged buffer (load/allocate as needed), then
        // discard the tile bytes (only the sum + the hash live on).
        self.fix_budget(edge, edge);
        let added = total_count(&tile);
        {
            let merged = self.merged_mut(&key, edge, edge);
            // Defensive: a tile of mismatched dims (a sheep can't legitimately
            // produce one) would panic merge; the content-hash + fixed edge make
            // this unreachable, but guard rather than panic on hostile input.
            if merged.w == tile.w && merged.h == tile.h {
                merged.merge(&tile);
            }
        }
        let meta = self.meta.entry(key).or_insert_with(|| FrameMeta {
            folded: HashSet::new(),
            count: 0,
            dims: (edge, edge),
            needs_rebuild: false,
        });
        meta.folded.insert(piece.hash.clone());
        meta.count += added;
        true
    }

    /// Retract a tile by its content **hash** (§6 fraud retraction, hash-only
    /// path). Marks the hash removed so it can never re-enter the merge (the
    /// always-correct guarantee). Because raw tiles are DISCARDED, an
    /// already-folded fraudulent tile cannot be exactly subtracted here — affected
    /// frames are flagged `needs_rebuild` and exact subtraction is deferred to the
    /// Phase-2 pull-based frame rebuild (re-pull the frame's live pieces and
    /// re-sum). Returns `true` if any known frame had folded that hash.
    pub fn retract_hash(&mut self, hash: &str) -> bool {
        self.removed.insert(hash.to_string());
        let mut hit = false;
        for m in self.meta.values_mut() {
            if m.folded.contains(hash) {
                m.needs_rebuild = true;
                hit = true;
            }
        }
        hit
    }

    /// Retract a tile by subtracting a **provided** `Accum` (the caller holds the
    /// bytes — e.g. it decoded the fraudulent `PieceUpload`). Subtracts `accum`
    /// element-wise from every frame that folded its hash — EXACTLY (the merge
    /// inverse) — drops the hash from those frames' folded sets, decrements their
    /// counts, and marks the hash removed so it can't re-enter. Returns `true` if
    /// any frame held it. The hash is recomputed from `accum` (the accumulator
    /// hashes bytes, never a claimed hash).
    pub fn retract(&mut self, accum: &Accum) -> bool {
        let hash = hist_hash_hex(accum);
        self.removed.insert(hash.clone());
        // Frames that folded this hash (snapshot the keys to avoid borrow
        // conflicts while we mutate the resident buffers).
        let affected: Vec<FrameKey> = self
            .meta
            .iter()
            .filter(|(_, m)| m.folded.contains(&hash))
            .map(|(k, _)| k.clone())
            .collect();
        if affected.is_empty() {
            return false;
        }
        let sub = total_count(accum);
        for key in affected {
            let (w, h) = self.meta.get(&key).map(|m| m.dims).unwrap_or((accum.w, accum.h));
            {
                let merged = self.merged_mut(&key, w, h);
                if merged.w == accum.w && merged.h == accum.h && subtract_ok(merged, accum) {
                    for (cell, part) in merged.data.iter_mut().zip(accum.data.iter()) {
                        cell[0] -= part[0];
                        cell[1] -= part[1];
                        cell[2] -= part[2];
                        cell[3] -= part[3];
                    }
                }
            }
            if let Some(m) = self.meta.get_mut(&key) {
                m.folded.remove(&hash);
                m.count = m.count.saturating_sub(sub);
            }
        }
        true
    }

    /// Convenience: drive retraction from a slashed/disputed hash set (§6). Each
    /// hash is marked removed (future ingests stay no-ops) via the hash-only
    /// [`Accumulator::retract_hash`] — the dispute path has no tile bytes, so
    /// exact subtraction is deferred to the Phase-2 rebuild.
    pub fn apply_disputes<'a, I: IntoIterator<Item = &'a str>>(&mut self, disputed: I) {
        for h in disputed {
            self.retract_hash(h);
        }
    }

    /// The merged (summed) histogram for `(sheep, frame)`, or `None` if nothing
    /// has been folded for it. Loads it back from disk into the LRU if it had been
    /// evicted (dims known from the always-resident meta).
    pub fn merged_accum(&mut self, sheep_id: &str, frame: u32) -> Option<&Accum> {
        let key = (sheep_id.to_string(), frame);
        let (w, h) = self.meta.get(&key)?.dims;
        self.ensure_resident(&key, w, h);
        self.resident.get(&key).map(|r| &r.merged)
    }

    /// Total accumulated sample count across ALL of a sheep's folded frames — the
    /// density measure that grows as more `(idx, pass)` tiles arrive. Answered
    /// from the always-resident running counts (no merged-buffer load).
    pub fn total_count(&self, sheep_id: &str) -> u64 {
        self.meta
            .iter()
            .filter(|((s, _), _)| s == sheep_id)
            .map(|(_, m)| m.count)
            .sum()
    }

    /// Number of distinct (live) tiles folded for `(sheep, frame)` — the size of
    /// the always-resident folded-hash set.
    pub fn tile_count(&self, sheep_id: &str, frame: u32) -> usize {
        self.meta
            .get(&(sheep_id.to_string(), frame))
            .map_or(0, |m| m.folded.len())
    }

    /// Tone-map the merged frame to an RGBA8 image (`edge*edge*4` bytes) — the
    /// hook video / display consume (§1.1 accumulate→tonemap→video). Returns
    /// `None` if the frame is empty or the sheep's genome was never registered.
    pub fn tonemap(&mut self, sheep_id: &str, frame: u32) -> Option<Vec<u8>> {
        let genome = self.genomes.get(sheep_id)?.clone();
        let key = (sheep_id.to_string(), frame);
        let (w, h) = self.meta.get(&key)?.dims;
        self.ensure_resident(&key, w, h);
        let merged = &self.resident.get(&key)?.merged;
        // ss = 1: the engine renders tiles at edge x edge (ss=1), so the merged
        // histogram is at output resolution.
        Some(tonemap(merged, &genome, w, h, 1))
    }

    // ---- LRU + disk-spill internals -----------------------------------------

    /// Fix `max_frames` from the budget once the frame dims are known. The
    /// resident set is bounded by FRAME COUNT (not bytes) so eviction is a simple
    /// LRU pop: `max(1, budget_bytes / frame_bytes)`.
    fn fix_budget(&mut self, w: usize, h: usize) {
        if self.max_frames != 0 {
            return;
        }
        let frame_bytes = w * h * 32; // [u64;4] per cell
        let budget_bytes = self.ram_budget_mb.saturating_mul(1_000_000);
        self.max_frames = (budget_bytes / frame_bytes.max(1)).max(1);
    }

    /// Bump `key` to most-recently-used in the recency list.
    fn touch(&mut self, key: &FrameKey) {
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            let k = self.order.remove(pos);
            self.order.insert(0, k);
        } else {
            self.order.insert(0, key.clone());
        }
    }

    /// Evict least-recently-used resident frames until the resident set fits the
    /// budget. A dirty frame is flushed to disk before being dropped from RAM.
    fn evict_to_fit(&mut self) {
        let cap = self.max_frames.max(1);
        while self.resident.len() > cap {
            let Some(victim) = self.order.pop() else { break };
            if let Some(res) = self.resident.remove(&victim) {
                if res.dirty {
                    self.flush(&victim, &res.merged);
                }
            }
        }
    }

    /// Ensure `(sheep, frame)`'s merged buffer is resident, loading it from disk
    /// (or allocating an empty one if neither resident nor on disk) and touching
    /// it as most-recently-used. Caller must already know it has meta for `key`.
    fn ensure_resident(&mut self, key: &FrameKey, w: usize, h: usize) {
        if !self.resident.contains_key(key) {
            let merged = self.load(key, w, h).unwrap_or_else(|| Accum::new(w, h));
            self.resident
                .insert(key.clone(), Resident { merged, dirty: false });
        }
        self.touch(key);
        self.evict_to_fit();
    }

    /// Borrow `(sheep, frame)`'s merged buffer mutably for a fold/subtract,
    /// loading it back from disk if evicted, marking it dirty, and refreshing its
    /// LRU recency. Allocates an empty buffer if the frame is brand new.
    fn merged_mut(&mut self, key: &FrameKey, w: usize, h: usize) -> &mut Accum {
        self.ensure_resident(key, w, h);
        let res = self.resident.get_mut(key).expect("just ensured resident");
        res.dirty = true;
        &mut res.merged
    }

    /// On-disk path for a frame's spilled merged buffer:
    /// `<spill>/<sheep_hex>/<frame>.acc`.
    fn spill_path(&self, key: &FrameKey) -> PathBuf {
        self.spill_dir.join(&key.0).join(format!("{}.acc", key.1))
    }

    /// Flush a merged buffer to disk (`encode_accum` bytes). Best-effort: a write
    /// failure leaves the (now-dropped-from-RAM) frame absent on disk, so a later
    /// reload allocates an empty buffer — degraded, not corrupt. The folded-hash
    /// set / count stay resident regardless, so density accounting is unaffected.
    fn flush(&self, key: &FrameKey, merged: &Accum) {
        let path = self.spill_path(key);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&path, encode_accum(merged).into_bytes());
    }

    /// Load a previously-flushed merged buffer from disk, or `None` if it was
    /// never spilled (or the on-disk bytes are unreadable/wrong-size). Dims come
    /// from the always-resident meta.
    fn load(&self, key: &FrameKey, w: usize, h: usize) -> Option<Accum> {
        let bytes = std::fs::read(self.spill_path(key)).ok()?;
        let b64 = String::from_utf8(bytes).ok()?;
        decode_accum(&b64, w, h)
    }
}

/// True iff `whole` still contains `part` cell-by-cell (no channel underflows) —
/// re-exported convenience over flame-core's [`subtract_ok`], used to assert a
/// retraction's inverse property in tests.
pub fn contains(whole: &Accum, part: &Accum) -> bool {
    subtract_ok(whole, part)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::pass_seed_id;
    use crate::hist::encode_accum;
    use flame_core::chunked::render_batch;
    use flame_core::rng::Rng;
    use sheep_proto::msg::PieceUpload;
    use std::sync::atomic::{AtomicU64, Ordering};

    const EDGE: usize = 8; // small + fast
    const SPP: u32 = 2_000;

    /// A unique temp dir per accumulator under construction (no `tempfile`
    /// dev-dep, so we mint a process+counter-unique path under the OS temp dir).
    fn tmp_dir() -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("sheep-accum-test-{pid}-{n}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A fresh accumulator backed by a unique temp dir. `ram_mb` = 1 gives a tiny
    /// resident budget so eviction triggers across a handful of EDGE=8 frames.
    fn new_accum(ram_mb: usize) -> Accumulator {
        Accumulator::new(tmp_dir(), ram_mb)
    }

    fn test_genome() -> Genome {
        let mut rng = Rng::new(2);
        Genome::random(&mut rng, 3)
    }

    /// Build a PieceUpload for `(frame, idx, pass)` whose hash is the TRUE hash
    /// of its bytes (an honest piece). `sheep` is an arbitrary 32-byte id.
    fn honest_piece(g: &Genome, sheep: &[u8; 32], frame: u32, idx: u32, pass: u32) -> PieceUpload {
        let seed_id = pass_seed_id(sheep, pass);
        let accum = render_batch(g, &seed_id, frame, idx, EDGE, EDGE, 1, SPP, 128);
        PieceUpload {
            sheep_id: hex(sheep),
            frame,
            idx,
            pass,
            hash: hist_hash_hex(&accum),
            count: total_count(&accum).to_string(),
            hist_b64: encode_accum(&accum),
        }
    }

    fn hex(b: &[u8]) -> String {
        let mut s = String::new();
        for x in b {
            s.push_str(&format!("{x:02x}"));
        }
        s
    }

    /// A set of pieces ingested in ANY order yields the same merged hash
    /// (commutativity / order-independence — the CRDT property).
    #[test]
    fn ingest_order_independent() {
        let g = test_genome();
        let sheep = [7u8; 32];
        let sheep_hex = hex(&sheep);
        // Several tiles of frame 0: distinct idx, plus a second pass (density).
        let pieces = vec![
            honest_piece(&g, &sheep, 0, 0, 0),
            honest_piece(&g, &sheep, 0, 1, 0),
            honest_piece(&g, &sheep, 0, 2, 0),
            honest_piece(&g, &sheep, 0, 0, 1),
        ];

        let mut a = new_accum(1);
        for p in &pieces {
            assert!(a.ingest(p, EDGE));
        }
        let ha = hist_hash_hex(a.merged_accum(&sheep_hex, 0).unwrap());

        // Reverse order.
        let mut b = new_accum(1);
        for p in pieces.iter().rev() {
            assert!(b.ingest(p, EDGE));
        }
        let hb = hist_hash_hex(b.merged_accum(&sheep_hex, 0).unwrap());

        assert_eq!(ha, hb, "merged frame is independent of ingest order (CRDT)");
    }

    /// Re-ingesting the same piece (same content hash) is a no-op (idempotent).
    #[test]
    fn duplicate_piece_is_a_noop() {
        let g = test_genome();
        let sheep = [3u8; 32];
        let sheep_hex = hex(&sheep);
        let p = honest_piece(&g, &sheep, 0, 0, 0);

        let mut a = new_accum(1);
        assert!(a.ingest(&p, EDGE), "first ingest changes state");
        let before = hist_hash_hex(a.merged_accum(&sheep_hex, 0).unwrap());
        assert!(!a.ingest(&p, EDGE), "duplicate content-hash is a no-op");
        let after = hist_hash_hex(a.merged_accum(&sheep_hex, 0).unwrap());
        assert_eq!(before, after, "duplicate did not change the merged frame");
        assert_eq!(a.tile_count(&sheep_hex, 0), 1);
    }

    /// A piece whose claimed hash != hash(bytes) is rejected, with no merge.
    #[test]
    fn hash_mismatch_rejected() {
        let g = test_genome();
        let sheep = [5u8; 32];
        let sheep_hex = hex(&sheep);
        let mut p = honest_piece(&g, &sheep, 0, 0, 0);
        p.hash = "deadbeef".repeat(8); // 64 hex chars, but wrong
        let mut a = new_accum(1);
        assert!(!a.ingest(&p, EDGE), "tampered hash rejected");
        assert!(a.merged_accum(&sheep_hex, 0).is_none(), "nothing merged");
    }

    /// Removal undoes a contribution EXACTLY via the bytes-providing `retract`:
    /// after subtracting the dropped tile's `Accum`, the merged frame equals the
    /// sum of the remaining tiles, and the retracted hash can't re-enter.
    #[test]
    fn removal_undoes_contribution_exactly() {
        let g = test_genome();
        let sheep = [11u8; 32];
        let sheep_hex = hex(&sheep);
        let keep = honest_piece(&g, &sheep, 0, 0, 0);
        let drop = honest_piece(&g, &sheep, 0, 1, 0);

        // Full accumulator: keep + drop.
        let mut full = new_accum(1);
        assert!(full.ingest(&keep, EDGE));
        assert!(full.ingest(&drop, EDGE));
        let merged_full = full.merged_accum(&sheep_hex, 0).unwrap().clone();

        // Reference: only `keep`.
        let mut only_keep = new_accum(1);
        assert!(only_keep.ingest(&keep, EDGE));
        let merged_keep = only_keep.merged_accum(&sheep_hex, 0).unwrap().clone();

        // The dropped tile is contained in the full merge (merge inverse exists).
        let dropped_accum = decode_accum(&drop.hist_b64, EDGE, EDGE).unwrap();
        assert!(contains(&merged_full, &dropped_accum), "drop ⊆ full merge");

        // Retract `drop` by PROVIDING its bytes: the merged frame must now equal
        // `keep` alone exactly.
        assert!(full.retract(&dropped_accum), "retract subtracts the folded tile");
        let merged_after = full.merged_accum(&sheep_hex, 0).unwrap();
        assert_eq!(
            hist_hash_hex(merged_after),
            hist_hash_hex(&merged_keep),
            "removal undoes the contribution exactly"
        );
        assert_eq!(full.tile_count(&sheep_hex, 0), 1, "dropped hash left the folded set");

        // And re-ingesting the retracted piece stays a no-op (fraud bar holds).
        assert!(!full.ingest(&drop, EDGE), "retracted hash cannot re-enter");
    }

    /// `retract_hash` (the §6 dispute path — no bytes) bars re-ingest of that hash.
    #[test]
    fn retract_hash_bars_reingest() {
        let g = test_genome();
        let sheep = [23u8; 32];
        let p = honest_piece(&g, &sheep, 0, 0, 0);

        let mut a = new_accum(1);
        assert!(a.ingest(&p, EDGE), "first ingest folds the tile");
        assert!(a.retract_hash(&p.hash), "retract_hash hits the folded frame");
        assert!(!a.ingest(&p, EDGE), "retracted hash cannot re-enter");

        // A hash never seen can still be pre-emptively barred (returns false, no
        // frame held it, but future ingest is barred).
        let q = honest_piece(&g, &sheep, 1, 0, 0);
        assert!(!a.retract_hash(&q.hash), "no frame folded q yet");
        assert!(!a.ingest(&q, EDGE), "pre-barred hash is rejected on ingest");
    }

    /// tonemap produces a non-empty RGBA image of the right size.
    #[test]
    fn tonemap_produces_image() {
        let g = test_genome();
        let sheep = [13u8; 32];
        let sheep_hex = hex(&sheep);
        let mut a = new_accum(1);
        a.register_sheep(&sheep_hex, g.clone());
        // Ingest a couple of tiles so the frame has density.
        assert!(a.ingest(&honest_piece(&g, &sheep, 0, 0, 0), EDGE));
        assert!(a.ingest(&honest_piece(&g, &sheep, 0, 1, 0), EDGE));

        let img = a.tonemap(&sheep_hex, 0).expect("tonemap a non-empty frame");
        assert_eq!(img.len(), EDGE * EDGE * 4, "RGBA8 at edge x edge");
        // Non-empty: at least some pixel differs from a flat zero buffer.
        assert!(img.iter().any(|&b| b != 0), "image is not all-zero");
    }

    /// total_count grows as more (idx, pass) tiles are ingested — density rises.
    #[test]
    fn total_count_grows_with_passes() {
        let g = test_genome();
        let sheep = [17u8; 32];
        let sheep_hex = hex(&sheep);
        let mut a = new_accum(1);
        a.ingest(&honest_piece(&g, &sheep, 0, 0, 0), EDGE);
        let c1 = a.total_count(&sheep_hex);
        // A second pass over the same (frame, idx) is a distinct tile.
        a.ingest(&honest_piece(&g, &sheep, 0, 0, 1), EDGE);
        let c2 = a.total_count(&sheep_hex);
        assert!(c2 > c1, "a second pass raised total accumulated count: {c1} -> {c2}");
    }

    /// With a tiny RAM budget, ingesting tiles across MORE frames than fit
    /// resident forces eviction; a later access of an evicted frame must reload
    /// it from disk byte-identically (unchanged by the spill round-trip).
    #[test]
    fn lru_eviction_and_disk_reload() {
        let g = test_genome();
        let sheep = [29u8; 32];
        let sheep_hex = hex(&sheep);

        // EDGE=8 frame = 8*8*32 = 2048 bytes; budget 1 MB → max_frames huge, so
        // force a 1-frame resident cap by using a per-frame budget below one
        // frame: ram_mb=0 would give budget 0, clamped to max(1) frame resident.
        let mut a = Accumulator::new(tmp_dir(), 0);

        const N_FRAMES: u32 = 5;
        // Fold two distinct tiles per frame across more frames than fit resident.
        for f in 0..N_FRAMES {
            assert!(a.ingest(&honest_piece(&g, &sheep, f, 0, 0), EDGE));
            assert!(a.ingest(&honest_piece(&g, &sheep, f, 1, 0), EDGE));
        }
        // Only one frame can be resident at a time (max_frames clamped to 1), so
        // the earlier frames must have been spilled to disk.
        assert!(a.resident.len() <= 1, "resident set is bounded to the budget");

        // Reference: a freshly-merged frame 0 (its own accumulator, no eviction
        // pressure since it holds a single frame).
        let mut ref_acc = new_accum(64);
        ref_acc.ingest(&honest_piece(&g, &sheep, 0, 0, 0), EDGE);
        ref_acc.ingest(&honest_piece(&g, &sheep, 0, 1, 0), EDGE);
        let want = hist_hash_hex(ref_acc.merged_accum(&sheep_hex, 0).unwrap());

        // Access frame 0 — it was evicted, so this must reload it from disk and
        // produce the SAME merged hash (eviction is lossless).
        let got = hist_hash_hex(a.merged_accum(&sheep_hex, 0).unwrap());
        assert_eq!(got, want, "evicted frame reloads byte-identically from disk");

        // tonemap of an evicted frame also works (reload + palette).
        a.register_sheep(&sheep_hex, g.clone());
        let img = a.tonemap(&sheep_hex, N_FRAMES - 1).expect("tonemap evicted frame");
        assert_eq!(img.len(), EDGE * EDGE * 4);

        // Counts are unaffected by eviction (always-resident meta).
        assert_eq!(a.tile_count(&sheep_hex, 0), 2);
    }
}
