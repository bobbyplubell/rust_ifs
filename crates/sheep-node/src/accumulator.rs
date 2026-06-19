//! The **accumulate** capability (ARCHITECTURE v3 §1.1, §5) — the heavy-data
//! layer, a struct SEPARATE from the pure [`crate::engine::Engine`].
//!
//! A worker renders tiles and uploads each as a [`PieceUpload`] (a compressed
//! tile histogram, §5). An [`Accumulator`] ingests those pieces and maintains,
//! per `(sheep, frame)`, the **element-wise integer sum** of all that frame's
//! `(idx, pass)` tile histograms — the dense, progressively-resolving frame
//! image that video / display consume (`tonemap`).
//!
//! ## Why this is a CRDT (§1.1)
//!
//! flame-core histograms are **content-addressed integer buffers** whose merge
//! (`Accum::merge`) is element-wise `u64` addition — commutative and
//! associative. The accumulator stores each ingested tile keyed by its
//! **content hash**, so:
//!
//! - **commutative + associative**: the merged frame is a sum, independent of
//!   ingest order;
//! - **idempotent**: re-ingesting a piece with a hash already present is a
//!   no-op (the same content can't be double-counted);
//! - **convergent**: any set of accumulators that ingest the same confirmed-piece
//!   set reach byte-identical state with zero coordination.
//!
//! Fraud retraction (§6) is a **keyed removal**: marking a content-hash slashed
//! subtracts exactly that tile's contribution from every frame it appears in,
//! and the result still converges (removal is itself commutative).
//!
//! ## Trust
//!
//! Ingest verifies `piece.hash == hist_hash_hex(decoded)` and rejects a mismatch
//! with **no render** — the accumulator never trusts a claimed hash, it hashes
//! the bytes. (Peer/gateway audit, §6, is the engine's job; the accumulator's
//! own gate is content-integrity.)

use std::collections::{HashMap, HashSet};

use flame_core::chunked::{hist_hash_hex, subtract_ok, total_count};
use flame_core::genome::Genome;
use flame_core::render::{tonemap, Accum};

use crate::hist::decode_accum;

/// Key for one frame's accumulation: `(sheep_id hex, frame)`.
type FrameKey = (String, u32);

/// One frame's content-addressed tile store + its cached merged sum.
struct FrameAccum {
    /// Tile histograms keyed by content hash (the CRDT's content-addressing).
    /// `(idx, pass)` is carried for diagnostics only — identity is the hash.
    tiles: HashMap<String, Tile>,
    /// Cached element-wise sum of all *live* (non-removed) tiles. `None` until
    /// first built / after an invalidating change.
    merged: Option<Accum>,
    /// Histogram dimensions (edge x edge) for this sheep's tiles.
    dims: (usize, usize),
}

struct Tile {
    accum: Accum,
    #[allow(dead_code)]
    idx: u32,
    #[allow(dead_code)]
    pass: u32,
}

/// The accumulate-capability node role (§1.1). Holds one CRDT frame-sum per
/// `(sheep, frame)` and the tonemap hook video/display read.
pub struct Accumulator {
    frames: HashMap<FrameKey, FrameAccum>,
    /// Sheep genomes (needed only for `tonemap`'s palette/gamma). Populated via
    /// [`Accumulator::register_sheep`].
    genomes: HashMap<String, Genome>,
    /// Content hashes retracted as fraud (§6). A tile whose hash is in here is
    /// excluded from every merged frame; re-ingesting it stays a no-op.
    removed: HashSet<String>,
}

impl Default for Accumulator {
    fn default() -> Self {
        Self::new()
    }
}

impl Accumulator {
    pub fn new() -> Self {
        Accumulator {
            frames: HashMap::new(),
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
    /// (duplicate content-hash, or a retracted hash).
    ///
    /// `edge` is the tile's pixel edge (`resolution.edge()`), needed to decode
    /// the flat histogram back into a 2-D `Accum`.
    pub fn ingest(&mut self, piece: &sheep_proto::msg::PieceUpload, edge: usize) -> bool {
        // Retracted content never re-enters (idempotent under fraud removal).
        if self.removed.contains(&piece.hash) {
            return false;
        }

        // Decode the heavy artifact, then VERIFY content-integrity: the stored
        // hash must equal the hash of the bytes. No render, no trust of the
        // claimed hash.
        let Some(accum) = decode_accum(&piece.hist_b64, edge, edge) else {
            return false;
        };
        if hist_hash_hex(&accum) != piece.hash {
            return false;
        }

        let key: FrameKey = (piece.sheep_id.clone(), piece.frame);
        let frame = self.frames.entry(key).or_insert_with(|| FrameAccum {
            tiles: HashMap::new(),
            merged: None,
            dims: (edge, edge),
        });

        // Idempotent by content-hash: re-ingesting the same tile is a no-op.
        if frame.tiles.contains_key(&piece.hash) {
            return false;
        }
        frame.tiles.insert(
            piece.hash.clone(),
            Tile {
                accum,
                idx: piece.idx,
                pass: piece.pass,
            },
        );
        frame.merged = None; // invalidate the cached sum
        true
    }

    /// Retract a tile by its content hash (§6 fraud retraction — a keyed CRDT
    /// removal). Subtracts that tile's contribution from every frame it is in
    /// and bars it from re-ingest. Returns `true` if any frame held it.
    pub fn retract(&mut self, hash: &str) -> bool {
        self.removed.insert(hash.to_string());
        let mut hit = false;
        for frame in self.frames.values_mut() {
            if frame.tiles.remove(hash).is_some() {
                frame.merged = None;
                hit = true;
            }
        }
        hit
    }

    /// Convenience: drive retraction from a slashed/disputed hash set (§6). Any
    /// currently-held tile whose hash is in `disputed` is removed; future
    /// ingests of those hashes stay no-ops.
    pub fn apply_disputes<'a, I: IntoIterator<Item = &'a str>>(&mut self, disputed: I) {
        for h in disputed {
            self.retract(h);
        }
    }

    /// The merged (summed) histogram for `(sheep, frame)`, or `None` if nothing
    /// has been ingested for it. Rebuilds the cached sum on demand.
    pub fn merged_accum(&mut self, sheep_id: &str, frame: u32) -> Option<&Accum> {
        let key = (sheep_id.to_string(), frame);
        let fa = self.frames.get_mut(&key)?;
        if fa.tiles.is_empty() {
            return None;
        }
        if fa.merged.is_none() {
            fa.merged = Some(build_merged(fa));
        }
        fa.merged.as_ref()
    }

    /// Total accumulated sample count across ALL of a sheep's ingested frames —
    /// the density measure that grows as more `(idx, pass)` tiles arrive.
    pub fn total_count(&self, sheep_id: &str) -> u64 {
        self.frames
            .iter()
            .filter(|((s, _), _)| s == sheep_id)
            .flat_map(|(_, fa)| fa.tiles.values())
            .map(|t| total_count(&t.accum))
            .sum()
    }

    /// Number of distinct (live) tiles held for `(sheep, frame)`.
    pub fn tile_count(&self, sheep_id: &str, frame: u32) -> usize {
        self.frames
            .get(&(sheep_id.to_string(), frame))
            .map_or(0, |fa| fa.tiles.len())
    }

    /// Tone-map the merged frame to an RGBA8 image (`edge*edge*4` bytes) — the
    /// hook video / display consume (§1.1 accumulate→tonemap→video). Returns
    /// `None` if the frame is empty or the sheep's genome was never registered.
    pub fn tonemap(&mut self, sheep_id: &str, frame: u32) -> Option<Vec<u8>> {
        let genome = self.genomes.get(sheep_id)?.clone();
        let key = (sheep_id.to_string(), frame);
        let fa = self.frames.get_mut(&key)?;
        if fa.tiles.is_empty() {
            return None;
        }
        let (w, h) = fa.dims;
        if fa.merged.is_none() {
            fa.merged = Some(build_merged(fa));
        }
        let merged = fa.merged.as_ref().unwrap();
        // ss = 1: the engine renders tiles at edge x edge (ss=1), so the merged
        // histogram is at output resolution.
        Some(tonemap(merged, &genome, w, h, 1))
    }
}

/// Build the element-wise integer sum of a frame's live tiles. Order-independent
/// (integer addition is commutative + associative), so the result is the CRDT's
/// convergent value regardless of ingest order. Iterating the hash map in
/// arbitrary order is therefore safe — the sum is identical.
fn build_merged(fa: &FrameAccum) -> Accum {
    let (w, h) = fa.dims;
    let mut acc = Accum::new(w, h);
    for tile in fa.tiles.values() {
        // Defensive: tiles of mismatched dims (a sheep can't legitimately
        // produce them) would panic merge; the content-hash + fixed edge make
        // this unreachable, but guard rather than panic on hostile input.
        if tile.accum.w == acc.w && tile.accum.h == acc.h {
            acc.merge(&tile.accum);
        }
    }
    acc
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

    const EDGE: usize = 8; // small + fast
    const SPP: u32 = 2_000;

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

        let mut a = Accumulator::new();
        for p in &pieces {
            assert!(a.ingest(p, EDGE));
        }
        let ha = hist_hash_hex(a.merged_accum(&sheep_hex, 0).unwrap());

        // Reverse order.
        let mut b = Accumulator::new();
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

        let mut a = Accumulator::new();
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
        let mut a = Accumulator::new();
        assert!(!a.ingest(&p, EDGE), "tampered hash rejected");
        assert!(a.merged_accum(&sheep_hex, 0).is_none(), "nothing merged");
    }

    /// Removal undoes a contribution EXACTLY: after retracting one tile the
    /// merged frame equals the sum of the remaining tiles, and the retracted
    /// tile is a subset of the pre-retraction merge (subtract_ok / merge inverse).
    #[test]
    fn removal_undoes_contribution_exactly() {
        let g = test_genome();
        let sheep = [11u8; 32];
        let sheep_hex = hex(&sheep);
        let keep = honest_piece(&g, &sheep, 0, 0, 0);
        let drop = honest_piece(&g, &sheep, 0, 1, 0);

        // Full accumulator: keep + drop.
        let mut full = Accumulator::new();
        assert!(full.ingest(&keep, EDGE));
        assert!(full.ingest(&drop, EDGE));
        let merged_full = full.merged_accum(&sheep_hex, 0).unwrap().clone();

        // Reference: only `keep`.
        let mut only_keep = Accumulator::new();
        assert!(only_keep.ingest(&keep, EDGE));
        let merged_keep = only_keep.merged_accum(&sheep_hex, 0).unwrap().clone();

        // The dropped tile is contained in the full merge (merge inverse exists).
        let dropped_accum = decode_accum(&drop.hist_b64, EDGE, EDGE).unwrap();
        assert!(contains(&merged_full, &dropped_accum), "drop ⊆ full merge");

        // Retract `drop`: the merged frame must now equal `keep` alone exactly.
        assert!(full.retract(&drop.hash), "retract removes the held tile");
        let merged_after = full.merged_accum(&sheep_hex, 0).unwrap();
        assert_eq!(
            hist_hash_hex(merged_after),
            hist_hash_hex(&merged_keep),
            "removal undoes the contribution exactly"
        );

        // And re-ingesting the retracted piece stays a no-op (fraud bar holds).
        assert!(!full.ingest(&drop, EDGE), "retracted hash cannot re-enter");
    }

    /// tonemap produces a non-empty RGBA image of the right size.
    #[test]
    fn tonemap_produces_image() {
        let g = test_genome();
        let sheep = [13u8; 32];
        let sheep_hex = hex(&sheep);
        let mut a = Accumulator::new();
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
        let mut a = Accumulator::new();
        a.ingest(&honest_piece(&g, &sheep, 0, 0, 0), EDGE);
        let c1 = a.total_count(&sheep_hex);
        // A second pass over the same (frame, idx) is a distinct tile.
        a.ingest(&honest_piece(&g, &sheep, 0, 0, 1), EDGE);
        let c2 = a.total_count(&sheep_hex);
        assert!(c2 > c1, "a second pass raised total accumulated count: {c1} -> {c2}");
    }
}
