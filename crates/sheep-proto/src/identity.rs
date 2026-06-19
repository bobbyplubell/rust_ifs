//! Sheep identity binds resolution (ARCHITECTURE v3 §2.1).
//!
//! Resolution is chosen at mint from a fixed tier set and **bound into the
//! sheep's identity**: two resolutions of one genome are distinct sheep, and
//! every node renders a sheep at its declared tier (determinism intact). So the
//! identity is `SHA-256( sheep_id(genome) ‖ tier_tag )` — genome + resolution
//! together define the sheep.

use flame_core::canonical::{sheep_id, to_hex};
use flame_core::genome::Genome;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The fixed resolution tier set (§2.1): square `W=H`, supersample `SS=1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ResolutionTier {
    /// 384×384 — the v2 canonical resolution (`spec::W`).
    R384,
    /// 512×512.
    R512,
    /// 768×768.
    R768,
    /// 1024×1024.
    R1024,
}

impl ResolutionTier {
    /// Render dimensions `(W, H)` (square; pre-supersample, `SS=1`).
    pub fn dims(&self) -> (u32, u32) {
        match self {
            ResolutionTier::R384 => (384, 384),
            ResolutionTier::R512 => (512, 512),
            ResolutionTier::R768 => (768, 768),
            ResolutionTier::R1024 => (1024, 1024),
        }
    }

    /// The single-byte tag folded into the sheep identity. Stable wire value —
    /// do not renumber (it would re-key every sheep).
    pub fn tag(&self) -> u8 {
        match self {
            ResolutionTier::R384 => 0,
            ResolutionTier::R512 => 1,
            ResolutionTier::R768 => 2,
            ResolutionTier::R1024 => 3,
        }
    }

    /// The edge length in pixels (`W == H`).
    pub fn edge(&self) -> u32 {
        self.dims().0
    }

    /// Parse from an edge length (the fixed tier set).
    pub fn from_edge(edge: u32) -> Option<ResolutionTier> {
        match edge {
            384 => Some(ResolutionTier::R384),
            512 => Some(ResolutionTier::R512),
            768 => Some(ResolutionTier::R768),
            1024 => Some(ResolutionTier::R1024),
            _ => None,
        }
    }
}

/// The sheep's identity: `SHA-256( sheep_id(genome) ‖ [tier_tag] )`.
///
/// `sheep_id(genome)` is the genome's content address (flame-core); appending
/// the tier tag makes genome + resolution jointly define the sheep.
pub fn sheep_identity(genome: &Genome, tier: ResolutionTier) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(sheep_id(genome));
    hasher.update([tier.tag()]);
    hasher.finalize().into()
}

/// [`sheep_identity`] as lowercase hex (64 chars).
pub fn sheep_identity_hex(genome: &Genome, tier: ResolutionTier) -> String {
    to_hex(&sheep_identity(genome, tier))
}
