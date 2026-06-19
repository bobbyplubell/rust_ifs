//! The `derive` rules (ARCHITECTURE v3 §2.1) — the deterministic, verifiable
//! functions that pin the valid genome space.
//!
//! A sheep's genome can only be *derived*, never authored:
//!   - **Mint:** `genome = derive(hash(timestamp_micros ‖ minter_pubkey))` —
//!     [`derive_minted`].
//!   - **Breed:** `genome = derive(parent_a, parent_b, seed)` —
//!     [`derive_bred`] (== `flame_core::breed`).
//!
//! Both reproduce a byte-identical genome (hence the same `sheep_id`) for the
//! same inputs on any target — that is the whole point: anyone re-derives and
//! verifies `genome == derive(...)` without trusting the minter.

use flame_core::breed::breed;
use flame_core::genome::Genome;
use flame_core::rng::Rng;
use sha2::{Digest, Sha256};

/// Transform count for a minted (brand-new) genome.
///
/// `Genome::random(rng, n)` takes the transform count; coordinator production
/// paths (`ga.rs` seed + `/breed` immigrants, `render`/`video`/`disk` fixtures)
/// all use `n = 3` for fresh genomes, so we match that. (`Genome::random` may
/// then add symmetry/final transforms, exactly as in v2.)
pub const MINT_TRANSFORMS: usize = 3;

/// Derive a bred genome: `flame_core::breed(a, b, seed)`.
///
/// Crossover + mutation + auto-frame, all threaded through one
/// `Rng::new(seed)`, so the child is a pure function of `(a, b, seed)`.
pub fn derive_bred(a: &Genome, b: &Genome, seed: u64) -> Genome {
    breed(a, b, seed)
}

/// The u64 RNG seed a mint derives from: the first 8 bytes (little-endian) of
/// `SHA-256( ts_micros.to_le_bytes() ‖ minter_pub )`.
///
/// Split out so callers/tests can inspect the seed independently of the genome.
pub fn mint_seed(ts_micros: u64, minter_pub: &[u8]) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(ts_micros.to_le_bytes());
    hasher.update(minter_pub);
    let digest = hasher.finalize();
    let mut seed_bytes = [0u8; 8];
    seed_bytes.copy_from_slice(&digest[..8]);
    u64::from_le_bytes(seed_bytes)
}

/// Derive a minted genome: seed an `Rng` from
/// `SHA-256( ts_micros ‖ minter_pub )` and roll `Genome::random`.
///
/// The pubkey kills collisions even at identical microseconds; `ts_micros` and
/// `minter_pub` are recorded in the signed [`crate::msg::Mint`] event so anyone
/// re-derives and verifies the genome.
pub fn derive_minted(ts_micros: u64, minter_pub: &[u8]) -> Genome {
    let mut rng = Rng::new(mint_seed(ts_micros, minter_pub));
    Genome::random(&mut rng, MINT_TRANSFORMS)
}
