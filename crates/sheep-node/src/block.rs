//! Canonical work-unit enumeration + block slicing (ARCHITECTURE v3 §4).
//!
//! Rendering is neutral: a peer claims a **block**, a fixed-size contiguous
//! slice of one sheep's canonical `(idx, frame, pass)` enumeration. To make a
//! block id tiny to gossip and locally re-derivable by anyone, the enumeration
//! is a single deterministic flattening:
//!
//! ```text
//! flat = 0;
//! for pass in 0.. {
//!     for idx in 0..IDXS_PER_FRAME {
//!         for frame in 0..N_FRAMES {
//!             unit[flat] = (frame, idx, pass);
//!             flat += 1;
//!         }
//!     }
//! }
//! ```
//!
//! **Breadth-first ordering (full animation visible sooner).** `frame` is the
//! FAST dimension and `idx` the middle one, so the canonical order sweeps
//! `idx == 0` across *every* frame `0..N_FRAMES` before it ever advances to
//! `idx == 1` — and only after all idxs of a pass are laid down does the next
//! pass deepen density. Since the least-covered block selection (engine
//! `pick_block` / `pick_blocks_for`) walks block indices `0, 1, 2, …` upward,
//! this makes the whole 0..128-frame animation acquire low-density coverage
//! quickly (boomerang playback looks alive almost immediately) and density then
//! deepens uniformly — rather than the old depth-first order (`idx` fast, `frame`
//! middle), which rendered all 64 idxs of frame 0 before frame 1 started, so only
//! ~7 of 128 frames had any data and the loop looked blank. The change is *only*
//! advisory ordering: a unit's identity is still `(frame, idx, pass)`, so
//! confirmed-coverage accounting, claims/collision-avoidance (§4), and
//! convergence are untouched — they key on the tuple, never the flat index.
//!
//! **Block K** owns the flat units `[K*BUNDLE_SIZE .. (K+1)*BUNDLE_SIZE)`.
//! Work is unbounded (passes never stop, §4), so block indices are unbounded
//! too — there is always a surplus of open blocks vs. active clients.

use crate::spec::{BUNDLE_SIZE, IDXS_PER_FRAME, N_FRAMES};

/// A canonical work unit inside one sheep: which animation `frame`, which
/// `idx` within that frame, and which `pass` (sample-density round).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Unit {
    pub frame: u32,
    pub idx: u32,
    pub pass: u32,
}

/// A block id: the sheep it belongs to (its 32-byte identity) + the block
/// index within that sheep's canonical enumeration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockId {
    pub sheep_identity: [u8; 32],
    pub block_index: u64,
}

/// Number of distinct `(frame, idx)` units in one pass.
pub const UNITS_PER_PASS: u64 = (N_FRAMES as u64) * (IDXS_PER_FRAME as u64);

/// Map a flat unit index to its `(frame, idx, pass)`. Breadth-first within a
/// pass: `frame` is the fast dimension, `idx` the middle one, so consecutive
/// flat indices walk across all frames at one idx before advancing the idx.
pub fn flat_to_unit(flat: u64) -> Unit {
    let pass = (flat / UNITS_PER_PASS) as u32;
    let within = flat % UNITS_PER_PASS;
    let idx = (within / N_FRAMES as u64) as u32;
    let frame = (within % N_FRAMES as u64) as u32;
    Unit { frame, idx, pass }
}

/// Map a `(frame, idx, pass)` back to its flat unit index. The inverse of
/// [`flat_to_unit`] (breadth-first: `frame` fast, `idx` middle, `pass` slow).
/// (frame, idx must be in range; pass is unbounded.)
pub fn unit_to_flat(u: Unit) -> u64 {
    debug_assert!(u.frame < N_FRAMES, "frame out of range");
    debug_assert!(u.idx < IDXS_PER_FRAME, "idx out of range");
    (u.pass as u64) * UNITS_PER_PASS
        + (u.idx as u64) * (N_FRAMES as u64)
        + (u.frame as u64)
}

/// The flat unit range `[start, end)` a block index owns.
pub fn block_flat_range(block_index: u64) -> (u64, u64) {
    let start = block_index * BUNDLE_SIZE as u64;
    (start, start + BUNDLE_SIZE as u64)
}

/// The work units of a block, in canonical order.
pub fn block_units(block: BlockId) -> Vec<Unit> {
    let (start, end) = block_flat_range(block.block_index);
    (start..end).map(flat_to_unit).collect()
}

/// Which block index a flat unit falls in.
pub fn flat_to_block_index(flat: u64) -> u64 {
    flat / BUNDLE_SIZE as u64
}

/// Which block index a `(frame, idx, pass)` unit falls in (the inverse path:
/// unit → flat → block).
pub fn unit_to_block_index(u: Unit) -> u64 {
    flat_to_block_index(unit_to_flat(u))
}

// ---- block id <-> wire string ----------------------------------------------
//
// `Claim.block_id` / `Heartbeat.block_id` are strings on the wire (§10). We use
// a structured, human-debuggable form `"<sheep_hex>:<block_index>"` — stable,
// round-trips, and trivially comparable.

/// Lowercase-hex sheep identity (matches flame-core / envelope hex).
fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn decode_hex_32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    let bytes = s.as_bytes();
    let nibble = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    for i in 0..32 {
        let hi = nibble(bytes[i * 2])?;
        let lo = nibble(bytes[i * 2 + 1])?;
        out[i] = (hi << 4) | lo;
    }
    Some(out)
}

impl BlockId {
    /// The wire string form: `"<sheep_identity_hex>:<block_index>"`.
    pub fn to_wire(&self) -> String {
        format!("{}:{}", hex_lower(&self.sheep_identity), self.block_index)
    }

    /// Parse a wire string back into a `BlockId`. Returns `None` on any
    /// malformed input.
    pub fn from_wire(s: &str) -> Option<BlockId> {
        let (hex, idx) = s.split_once(':')?;
        let sheep_identity = decode_hex_32(hex)?;
        let block_index = idx.parse::<u64>().ok()?;
        Some(BlockId {
            sheep_identity,
            block_index,
        })
    }

    /// The sheep identity as lowercase hex.
    pub fn sheep_hex(&self) -> String {
        hex_lower(&self.sheep_identity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_unit_roundtrips() {
        // Cover several passes and the boundaries within a pass.
        for flat in [0u64, 1, 63, 64, 65, UNITS_PER_PASS - 1, UNITS_PER_PASS, UNITS_PER_PASS + 7, 3 * UNITS_PER_PASS + 100] {
            let u = flat_to_unit(flat);
            assert!(u.frame < N_FRAMES && u.idx < IDXS_PER_FRAME);
            assert_eq!(unit_to_flat(u), flat, "flat {flat} did not round-trip");
        }
    }

    #[test]
    fn enumeration_order_is_frame_then_idx_then_pass() {
        // Breadth-first within a pass: `frame` increments fastest so the whole
        // animation gets idx-0 coverage before any frame deepens to idx 1.
        // flat 0 = (frame 0, idx 0, pass 0)
        assert_eq!(flat_to_unit(0), Unit { frame: 0, idx: 0, pass: 0 });
        // flat 1 = (frame 1, idx 0, pass 0)  -- frame increments fastest
        assert_eq!(flat_to_unit(1), Unit { frame: 1, idx: 0, pass: 0 });
        // flat N_FRAMES = (frame 0, idx 1, pass 0) -- idx increments next, only
        // after every frame has been touched at idx 0.
        assert_eq!(flat_to_unit(N_FRAMES as u64), Unit { frame: 0, idx: 1, pass: 0 });
        // flat UNITS_PER_PASS = (frame 0, idx 0, pass 1) -- pass increments last
        assert_eq!(flat_to_unit(UNITS_PER_PASS), Unit { frame: 0, idx: 0, pass: 1 });
    }

    #[test]
    fn block_units_then_back() {
        let sheep = [7u8; 32];
        for block_index in [0u64, 1, 5, 1000] {
            let block = BlockId { sheep_identity: sheep, block_index };
            let units = block_units(block);
            assert_eq!(units.len(), BUNDLE_SIZE);
            // every unit maps back to this block.
            for u in &units {
                assert_eq!(unit_to_block_index(*u), block_index, "unit {u:?} not in block {block_index}");
            }
            // and the flat range is exactly contiguous.
            let (start, end) = block_flat_range(block_index);
            assert_eq!(end - start, BUNDLE_SIZE as u64);
        }
    }

    #[test]
    fn block_id_wire_roundtrips() {
        let block = BlockId { sheep_identity: [0xabu8; 32], block_index: 42 };
        let wire = block.to_wire();
        assert_eq!(BlockId::from_wire(&wire), Some(block));
        assert_eq!(BlockId::from_wire("nope"), None);
        assert_eq!(BlockId::from_wire("ab:notanumber"), None);
    }
}
