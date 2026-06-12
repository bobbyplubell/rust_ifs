//! Canonical genome JSON and content addressing (`sheep_id`).
//!
//! The canonical byte form of a genome is `serde_json::to_string(&genome)`
//! after parsing the input into the `Genome` struct: struct-declaration field
//! order plus ryu shortest-roundtrip float formatting make this deterministic
//! across platforms. Canonicalize = parse → re-serialize. A sheep's identity
//! is the SHA-256 of those canonical bytes.

use sha2::{Digest, Sha256};

use crate::genome::Genome;

/// Serialize a genome to its canonical JSON string (no whitespace, struct
/// field order, ryu float formatting).
pub fn canonical_json(genome: &Genome) -> String {
    serde_json::to_string(genome).expect("genome serialization cannot fail")
}

/// Parse arbitrary genome JSON (any whitespace / key order) and re-serialize
/// it into canonical form.
pub fn canonicalize(json: &str) -> Result<String, String> {
    let genome: Genome =
        serde_json::from_str(json).map_err(|e| format!("bad genome json: {e}"))?;
    Ok(canonical_json(&genome))
}

/// SHA-256 of the canonical JSON bytes — the sheep's content address.
pub fn sheep_id(genome: &Genome) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(canonical_json(genome).as_bytes());
    hasher.finalize().into()
}

/// `sheep_id` as lowercase hex (64 chars).
pub fn sheep_id_hex(genome: &Genome) -> String {
    to_hex(&sheep_id(genome))
}

/// Lowercase hex encoding of arbitrary bytes.
pub fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::Rng;

    fn corpus() -> Vec<Genome> {
        [2u64, 3, 7, 11, 23, 42]
            .iter()
            .map(|&seed| {
                let mut rng = Rng::new(seed);
                Genome::random(&mut rng, 3 + (seed as usize % 4))
            })
            .collect()
    }

    #[test]
    fn canonicalize_is_idempotent() {
        for genome in corpus() {
            let once = canonical_json(&genome);
            let reparsed: Genome = serde_json::from_str(&once).expect("reparse");
            let twice = canonical_json(&reparsed);
            assert_eq!(once, twice, "parse → serialize must be a fixed point");
            // And one more round for good measure.
            let reparsed2: Genome = serde_json::from_str(&twice).expect("reparse 2");
            assert_eq!(twice, canonical_json(&reparsed2));
        }
    }

    #[test]
    fn whitespace_and_key_order_do_not_change_sheep_id() {
        for genome in corpus() {
            let canonical = canonical_json(&genome);
            let id = sheep_id_hex(&genome);

            // Whitespace variant: pretty-printed JSON of the same genome.
            let pretty = serde_json::to_string_pretty(&genome).unwrap();
            assert_ne!(pretty, canonical);
            let from_pretty: Genome = serde_json::from_str(&pretty).unwrap();
            assert_eq!(sheep_id_hex(&from_pretty), id);

            // Key-order variant: round-trip through serde_json::Value, whose
            // object maps are sorted alphabetically (different from struct
            // declaration order).
            let value: serde_json::Value = serde_json::from_str(&canonical).unwrap();
            let reordered = serde_json::to_string(&value).unwrap();
            assert_ne!(reordered, canonical, "Value round-trip should reorder keys");
            let from_reordered: Genome = serde_json::from_str(&reordered).unwrap();
            assert_eq!(sheep_id_hex(&from_reordered), id);

            // canonicalize() maps every variant back to the same bytes.
            assert_eq!(canonicalize(&pretty).unwrap(), canonical);
            assert_eq!(canonicalize(&reordered).unwrap(), canonical);
        }
    }

    #[test]
    fn sheep_id_is_hex_of_canonical_sha256() {
        let genome = &corpus()[0];
        let id = sheep_id_hex(genome);
        assert_eq!(id.len(), 64);
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
    }

    #[test]
    fn canonicalize_rejects_garbage() {
        assert!(canonicalize("{not json").is_err());
        assert!(canonicalize("{\"transforms\": []}").is_err());
    }
}
