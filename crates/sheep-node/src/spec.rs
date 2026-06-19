//! Render-spec constants for the engine.
//!
//! These mirror `coordinator/src/spec.rs` (ARCHITECTURE v3 §10.1). The
//! coordinator crate is the seed's HTTP half and is *not* a dependency of this
//! pure engine, so the small set of constants the work-distribution + economy
//! logic needs are restated here. They must stay numerically identical to the
//! coordinator's; the values are protocol constants (§10.1).

/// Animation loop length (frames). 128 = genesis loop length.
pub const N_FRAMES: u32 = 128;

/// Distinct render idxs per `(sheep, frame)`; once `IDXS_PER_FRAME` are merged a
/// frame is fully covered for that pass (more passes raise density).
pub const IDXS_PER_FRAME: u32 = 64;

/// Accepted tiles that earn one spendable credit (§3).
pub const TILES_PER_CREDIT: u64 = 128;

/// Work units in one block (§4).
pub const BUNDLE_SIZE: usize = 16;

/// Samples per tile (one work unit). Coordinator production value.
pub const SPP: u32 = 200_000;

// ---- credit sinks (§3) ------------------------------------------------------
//
// Credits source = rendering (`TILES_PER_CREDIT`). Sinks = vote / mint / breed.
// Voting is cheap + frequent ("your say"); mint is moderate (anti-spam, "a sheep
// has value", but accessible); breed is the costliest of births (§2.1). The real
// flock-bounding lever is age-escalating survival cost (§2.2/§3.1), not birth
// price — so these are kept modest-but-ordered: VOTE < MINT < BREED.

/// §3 vote sink: backing a sheep's survival costs one credit (cheap, frequent).
pub const VOTE_COST: u64 = 1;

/// §3 mint sink (§2.1 "moderate"): a brand-new sheep at the base (R384) tier.
/// Scaled up by the chosen resolution tier (`resolution_cost_mult`).
pub const MINT_COST: u64 = 8;

/// §3 breed sink (§2.1 "the costliest of births"): a sheep from two parents at
/// the base (R384) tier. Scaled up by the chosen resolution tier.
pub const BREED_COST: u64 = 20;

/// §2.1 resolution is a cost multiplier: higher tiers cost proportionally more
/// to mint/breed (and consume proportionally more life-support). Multiplier vs.
/// the R384 base, by edge-area ratio rounded to a clean integer ladder.
/// 384→1, 512→2, 768→4, 1024→8.
pub fn resolution_cost_mult(tier: sheep_proto::identity::ResolutionTier) -> u64 {
    use sheep_proto::identity::ResolutionTier::*;
    match tier {
        R384 => 1,
        R512 => 2,
        R768 => 4,
        R1024 => 8,
    }
}
