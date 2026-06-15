//! Render-spec constants. These pin the dimensions of every work unit the
//! coordinator hands out and re-renders, so the client's WASM render and the
//! coordinator's native re-render agree byte-for-byte.
//!
//! A sheep *could* carry its own spec (ARCHITECTURE mentions per-spec frame
//! counts); v2.0 uses one global spec for the whole flock and stores it per
//! sheep in the DB so a future migration can vary it without a protocol break.

/// Canonical render resolution (pre-supersample). 384×384 per ARCHITECTURE §2.
pub const W: u32 = 384;
pub const H: u32 = 384;

/// Linear supersample factor. ss=1 per ARCHITECTURE §2.
pub const SS: u32 = 1;

/// Samples per tile (one work unit). Small enough that re-rendering for
/// verification is cheap; large enough to be worth a round-trip.
pub const SPP: u32 = 200_000;

/// Animation loop length (frames). 128 = genesis loop length.
pub const N_FRAMES: u32 = 128;

/// How many distinct render idxs exist per (sheep, frame). The assignment
/// ledger hands these out without collision; once `IDXS_PER_FRAME` are merged a
/// frame is "fully covered" for this pass (more passes raise sample density).
pub const IDXS_PER_FRAME: u32 = 64;

/// Accepted tiles that earn one spendable credit.
pub const TILES_PER_CREDIT: u64 = 128;

/// Work units handed out per `/assign` bundle.
pub const BUNDLE_SIZE: usize = 16;

/// Credits spent to propose a breeding pairing.
pub const BREED_COST: i64 = 4;

/// Generation length in milliseconds. Lives server-side (clients read it from
/// `/api/flock`). 24h default; override with GEN_MS env.
pub const GEN_MS_DEFAULT: u64 = 24 * 60 * 60 * 1000;

/// Upper bound on a single hist payload after decompression, in bytes. A tile
/// histogram is `W*SS*H*SS*4*8` bytes; reject anything larger (untrusted input
/// safety — bounds the decode + verify cost).
pub const fn max_hist_bytes() -> usize {
    (W * SS) as usize * (H * SS) as usize * 4 * 8
}

/// Number of u64 cells in a tile histogram (`w*ss*h*ss*4`).
pub const fn hist_cells() -> usize {
    (W * SS) as usize * (H * SS) as usize * 4
}
