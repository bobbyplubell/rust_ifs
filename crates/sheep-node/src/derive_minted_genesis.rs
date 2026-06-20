//! A deterministic **genesis sheep** for the step-2 demo (ARCHITECTURE v3
//! §12-step-2: lifecycle/mint is step 5; this is a fixed demo birth so two peers
//! share a sheep to render).
//!
//! It is a real, valid [`Mint`] (§2.1): a node that receives it re-derives the
//! genome from the recorded `(ts_micros, minter_pub)` and verifies it, exactly
//! as it would any minted sheep — there is no special-casing in the engine. The
//! only thing "genesis" about it is that the `(ts, minter key)` are *fixed
//! constants*, so every node produces the byte-identical sheep identity and
//! both peers in the two-peer test render the same sheep.

use ed25519_dalek::SigningKey;
use sheep_proto::identity::ResolutionTier;
use sheep_proto::msg::Mint;
use sheep_proto::{proto, Envelope};

/// Fixed 32-byte seed for the genesis minter key. Not a real user — just a
/// constant so the genesis sheep is the same on every node.
const GENESIS_MINTER_SEED: [u8; 32] = [0x5e; 32]; // "5e" ~ "sheep"

/// Fixed mint timestamp (micros). With the fixed minter key this pins one
/// genome → one `sheep_id` for the demo.
const GENESIS_TS_MICROS: u64 = 1_700_000_000_000_000;

/// The genesis sheep's resolution. R384 = the cheapest tier (smallest render),
/// keeping the demo / two-peer test fast.
const GENESIS_TIER: ResolutionTier = ResolutionTier::R384;

/// The fixed genesis minter signing key.
fn genesis_minter_key() -> SigningKey {
    SigningKey::from_bytes(&GENESIS_MINTER_SEED)
}

/// Lowercase hex of a byte slice.
fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// The `index`-th genesis [`Mint`] envelope (`t = proto::FLOCK`), ready to
/// `engine.apply(..)`. Deterministic: same bytes on every node. The whole
/// genesis flock is derived from the fixed `(genesis key, ts, seq)` constants
/// varied only by `index`, so two nodes produce byte-identical founders WITHOUT
/// gossiping anything — the v4 cure for the v3 per-node boot-mint divergence.
pub fn genesis_mint_at(index: usize) -> Envelope {
    let key = genesis_minter_key();
    let minter_pub = hex_lower(&key.verifying_key().to_bytes());
    let ts_micros = GENESIS_TS_MICROS + index as u64; // distinct genome per founder
    let mint = Mint {
        ts_micros,
        minter_pub,
        resolution: GENESIS_TIER,
        // seq from the (constant) genesis minter key, which the engine
        // grandfathers past the §3 credit check (`genesis_minter_pub`). Not a
        // real user spend; `index` keeps the seqs distinct (no equivocation).
        seq: index as u64,
    };
    let body = serde_json::to_value(&mint).expect("Mint -> Value cannot fail");
    // birth_ms is ts_micros/1000 in the engine; sign with that ts so the
    // envelope's ts matches the recorded mint time.
    let mut env = Envelope::new(proto::FLOCK, "", ts_micros / 1000, body);
    env.sign(&key);
    env
}

/// The genesis flock of `count` deterministic founders (the §1 `N_base` cohort).
/// Every node applies these at boot; identical bytes everywhere → no divergence.
pub fn genesis_flock(count: usize) -> Vec<Envelope> {
    (0..count).map(genesis_mint_at).collect()
}

/// The first genesis [`Mint`] (back-compat shim for `index = 0`).
pub fn genesis_mint() -> Envelope {
    genesis_mint_at(0)
}

/// The genesis minter's public key, lowercase hex. A fixed protocol constant
/// (no private user behind it): the engine grandfathers this one key past the
/// §3 credit check so the bootstrap genesis birth applies on a fresh node that
/// has earned nothing yet.
pub fn genesis_minter_pub() -> String {
    hex_lower(&genesis_minter_key().verifying_key().to_bytes())
}

/// The `index`-th genesis sheep's identity hex. Re-derived the same way the
/// engine does (`apply_mint`), for registration / display / immortality checks.
pub fn genesis_sheep_hex_at(index: usize) -> String {
    use sheep_proto::derive::derive_minted;
    use sheep_proto::identity::sheep_identity;
    let key = genesis_minter_key();
    let minter = key.verifying_key().to_bytes();
    let genome = derive_minted(GENESIS_TS_MICROS + index as u64, &minter);
    hex_lower(&sheep_identity(&genome, GENESIS_TIER))
}

/// The identity hexes of the first `count` genesis founders.
pub fn genesis_sheep_hexes(count: usize) -> Vec<String> {
    (0..count).map(genesis_sheep_hex_at).collect()
}

/// The first genesis sheep's identity hex (back-compat shim for `index = 0`).
pub fn genesis_sheep_hex() -> String {
    genesis_sheep_hex_at(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn genesis_is_a_valid_deterministic_mint() {
        let a = genesis_mint();
        let b = genesis_mint();
        assert_eq!(a, b, "genesis must be byte-identical across calls");
        assert!(a.verify(), "genesis mint must carry a valid signature");
        assert_eq!(a.t, proto::FLOCK);

        // Applying it populates the flock at the published identity.
        let mut eng = crate::engine::Engine::new(SigningKey::from_bytes(&[1u8; 32]));
        assert!(eng.apply(&a, 1000), "engine accepts the genesis mint");
        assert!(
            eng.flock().contains_key(&genesis_sheep_hex()),
            "genesis sheep identity is in the flock after apply"
        );
    }
}
