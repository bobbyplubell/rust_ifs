//! Contract tests for `sheep-proto`.
//!
//! - `canonical()` byte-matches the `auth.rs::canonical_message` oracle on
//!   representative bodies (nested objects, arrays, numbers, the `count`
//!   string).
//! - sign → verify round-trips; a tampered body fails.
//! - `derive_minted` / `derive_bred` are deterministic against committed
//!   golden `sheep_id_hex` values.
//! - `is_equivocation` true for same `(from, seq)` different body, false
//!   otherwise.
//! - serde round-trip for every message body type.

use ed25519_dalek::SigningKey;
use flame_core::canonical::sheep_id_hex;
use flame_core::genome::Genome;
use flame_core::rng::Rng;
use serde_json::{json, Value};

use sheep_proto::derive::mint_seed;
use sheep_proto::msg::*;
use sheep_proto::{
    derive_bred, derive_minted, is_equivocation, sheep_identity_hex, Envelope, Equivocation,
    ResolutionTier, Seq,
};

// ---- the oracle: a verbatim copy of auth.rs::canonical_message ---------------
// (We replicate it rather than depend on the coordinator crate, exactly as the
// brief allows: "copy the oracle's logic path or call a small replica.")

fn oracle_sort_value(v: Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut sorted = serde_json::Map::new();
            let mut entries: Vec<(String, Value)> = map.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            for (k, val) in entries {
                sorted.insert(k, oracle_sort_value(val));
            }
            Value::Object(sorted)
        }
        Value::Array(arr) => Value::Array(arr.into_iter().map(oracle_sort_value).collect()),
        other => other,
    }
}

fn oracle_canonical_message(body: &Value) -> String {
    let mut clone = body.clone();
    if let Some(obj) = clone.as_object_mut() {
        obj.remove("sig");
    }
    let sorted = oracle_sort_value(clone);
    serde_json::to_string(&sorted).unwrap()
}

// ---- protocol-id registry ----------------------------------------------------

#[test]
fn flock_sync_proto_id_is_registered() {
    use sheep_proto::proto;
    // The §10-convergence flock-catch-up req/resp protocol id is the documented
    // string and is advertised in the full-node protocol set.
    assert_eq!(proto::FLOCK_SYNC, "/sheep/flock-sync/1.0.0");
    assert!(
        proto::ALL.contains(&proto::FLOCK_SYNC),
        "FLOCK_SYNC must be in the advertised protocol set"
    );
    // No duplicate protocol ids in the advertised set.
    let mut seen = std::collections::HashSet::new();
    for p in proto::ALL {
        assert!(seen.insert(*p), "duplicate protocol id in ALL: {p}");
    }
}

// ---- canonicalization byte-match --------------------------------------------

#[test]
fn canonical_byte_matches_oracle() {
    let bodies = vec![
        // Nested objects + key reordering + sig present (must be stripped).
        json!({
            "t": "vote",
            "from": "aa",
            "sig": "ff00",
            "nested": { "z": 1, "a": { "y": 2, "x": 3 } },
            "nonce": 17u64,
        }),
        // Arrays of objects (order preserved, elements sorted in place).
        json!({
            "results": [
                { "idx": 2, "frame": 0, "hash": "ab" },
                { "frame": 1, "idx": 5, "hash": "cd" },
            ],
            "pub": "bb",
        }),
        // Numbers: integers, the count STRING, booleans, null.
        json!({
            "count": "9007199254740993",   // > 2^53, kept as a string
            "n": 9007199254740991u64,      // exactly 2^53 - 1
            "neg": -42,
            "flag": true,
            "nothing": Value::Null,
        }),
        // An actual envelope serialized to Value.
        serde_json::to_value(&{
            let mut e = Envelope::new("vote", "aa", 123, json!({ "sheep_id": "deadbeef" }));
            e.sig = "should_be_stripped".into();
            e
        })
        .unwrap(),
    ];

    for body in bodies {
        assert_eq!(
            sheep_proto::canonicalize_value(&body),
            oracle_canonical_message(&body),
            "canonicalize_value must byte-match the auth.rs oracle"
        );
    }
}

#[test]
fn envelope_canonical_matches_oracle() {
    // The envelope's canonical() == oracle over its serialized Value.
    let mut e = Envelope::new(
        "piece",
        "ab",
        9,
        serde_json::to_value(PieceUpload {
            sheep_id: "id".into(),
            frame: 3,
            idx: 7,
            pass: 0,
            hash: "h".into(),
            count: "9007199254740993".into(),
            hist_b64: "AAAA".into(),
        })
        .unwrap(),
    );
    e.sig = "deadbeef".into();
    let as_value = serde_json::to_value(&e).unwrap();
    assert_eq!(e.canonical(), oracle_canonical_message(&as_value));
    // And sig really is excluded.
    assert!(!e.canonical().contains("deadbeef"));
    assert!(!e.canonical().contains("\"sig\""));
}

// ---- sign / verify ----------------------------------------------------------

#[test]
fn sign_verify_roundtrip_and_tamper() {
    let key = SigningKey::from_bytes(&[7u8; 32]);
    let mut e = Envelope::new("vote", "", 42, json!({ "sheep_id": "abc" }));
    e.sign(&key);

    assert!(e.verify(), "freshly signed envelope must verify");
    assert_eq!(e.from, hex::encode_local(&key.verifying_key().to_bytes()));

    // Tamper the body after signing.
    let mut tampered = e.clone();
    tampered.body = json!({ "sheep_id": "xyz" });
    assert!(!tampered.verify(), "tampered body must fail verify");

    // Tamper the header.
    let mut tampered2 = e.clone();
    tampered2.ts = 43;
    assert!(!tampered2.verify(), "tampered ts must fail verify");

    // Wrong-key signature: from says one key, sig from another.
    let mut e2 = Envelope::new("vote", "", 42, json!({ "sheep_id": "abc" }));
    e2.sign(&SigningKey::from_bytes(&[9u8; 32]));
    e2.from = e.from.clone();
    assert!(!e2.verify(), "from/sig mismatch must fail");
}

// hex helper for the test (the crate keeps its hex private).
mod hex {
    pub fn encode_local(bytes: &[u8]) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut out = String::with_capacity(bytes.len() * 2);
        for &b in bytes {
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
        out
    }
}

// ---- derive determinism (committed goldens) ---------------------------------

#[test]
fn derive_minted_is_deterministic_golden() {
    let ts: u64 = 1_700_000_000_000_000;
    let minter = [0xABu8; 32];

    let g1 = derive_minted(ts, &minter);
    let g2 = derive_minted(ts, &minter);
    assert_eq!(
        sheep_id_hex(&g1),
        sheep_id_hex(&g2),
        "same inputs must give identical sheep_id"
    );

    // Committed golden: regenerated from the seed path below; pins cross-target
    // determinism of the mint derivation.
    const GOLDEN_MINT: &str =
        "2b5dc7ce40becbc9c7bc872d0ea9b9f0049c681deae7552d53b9effa4ef4ff18";
    assert_eq!(sheep_id_hex(&g1), GOLDEN_MINT, "mint sheep_id drifted");

    // Different minter -> different sheep (collision resistance).
    let g3 = derive_minted(ts, &[0xCDu8; 32]);
    assert_ne!(sheep_id_hex(&g1), sheep_id_hex(&g3));
}

#[test]
fn derive_bred_is_deterministic_golden() {
    let a = {
        let mut r = Rng::new(7);
        Genome::random(&mut r, 4)
    };
    let b = {
        let mut r = Rng::new(3);
        Genome::random(&mut r, 5)
    };
    let seed = 0xDEAD_BEEFu64;

    let c1 = derive_bred(&a, &b, seed);
    let c2 = derive_bred(&a, &b, seed);
    assert_eq!(sheep_id_hex(&c1), sheep_id_hex(&c2));

    const GOLDEN_BRED: &str =
        "5bf3ceef963c91fd5514fd535c26bffc7520e0dbfb7182e04b2fec39991c3984";
    assert_eq!(sheep_id_hex(&c1), GOLDEN_BRED, "bred sheep_id drifted");

    // Different seed -> different child.
    assert_ne!(sheep_id_hex(&c1), sheep_id_hex(&derive_bred(&a, &b, seed + 1)));
}

#[test]
fn sheep_identity_binds_resolution() {
    let g = {
        let mut r = Rng::new(7);
        Genome::random(&mut r, 4)
    };
    let id384 = sheep_identity_hex(&g, ResolutionTier::R384);
    let id512 = sheep_identity_hex(&g, ResolutionTier::R512);
    assert_ne!(id384, id512, "two tiers of one genome are distinct sheep");
    assert_eq!(id384.len(), 64);

    // Stable golden for the 384 tier of this genome.
    const GOLDEN_IDENTITY_384: &str =
        "38124595da4bc42d155f70fc9a91c4524570d4a1ecba45afa618016cb5475928";
    assert_eq!(id384, GOLDEN_IDENTITY_384, "sheep_identity drifted");

    assert_eq!(ResolutionTier::R1024.dims(), (1024, 1024));
    assert_eq!(ResolutionTier::from_edge(768), Some(ResolutionTier::R768));
    assert_eq!(ResolutionTier::from_edge(999), None);
}

#[test]
fn mint_seed_is_first8_le_of_sha256() {
    // Spot-check the seed derivation against an explicit recompute.
    use sha2::{Digest, Sha256};
    let ts: u64 = 42;
    let minter = b"hello";
    let mut h = Sha256::new();
    h.update(ts.to_le_bytes());
    h.update(minter);
    let d = h.finalize();
    let mut s = [0u8; 8];
    s.copy_from_slice(&d[..8]);
    assert_eq!(mint_seed(ts, minter), u64::from_le_bytes(s));
}

// ---- equivocation -----------------------------------------------------------

#[test]
fn equivocation_detection() {
    let key = SigningKey::from_bytes(&[5u8; 32]);

    // Two claims, same key, same seq, DIFFERENT block -> equivocation.
    let mut a = Envelope::new("claim", "", 1, json!({ "block_id": "AAA", "seq": 9u64 }));
    a.sign(&key);
    let mut b = Envelope::new("claim", "", 2, json!({ "block_id": "BBB", "seq": 9u64 }));
    b.sign(&key);
    assert!(is_equivocation(&a, &b), "same (from,seq) different body");
    assert!(Equivocation::new(a.clone(), b.clone()).is_valid());

    // Same key, DIFFERENT seq -> not equivocation.
    let mut c = Envelope::new("claim", "", 3, json!({ "block_id": "CCC", "seq": 10u64 }));
    c.sign(&key);
    assert!(!is_equivocation(&a, &c), "different seq is not equivocation");

    // Different keys, same seq -> not equivocation.
    let mut d = Envelope::new("claim", "", 4, json!({ "block_id": "DDD", "seq": 9u64 }));
    d.sign(&SigningKey::from_bytes(&[6u8; 32]));
    assert!(!is_equivocation(&a, &d), "different signer is not equivocation");

    // Identical content (duplicate, same canonical bytes) -> not equivocation.
    let dup = a.clone();
    assert!(!is_equivocation(&a, &dup), "duplicate is not equivocation");

    // An unsigned/garbage envelope -> not equivocation.
    let mut bad = a.clone();
    bad.sig = "00".into();
    assert!(!is_equivocation(&bad, &b), "unverifiable message fails");
}

// ---- serde round-trips for every message type --------------------------------

fn roundtrip<T>(v: &T)
where
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let s = serde_json::to_string(v).unwrap();
    let back: T = serde_json::from_str(&s).unwrap();
    assert_eq!(v, &back, "serde round-trip mismatch for {s}");
}

#[test]
fn serde_roundtrip_all_messages() {
    roundtrip(&Mint {
        ts_micros: 123,
        minter_pub: "aa".into(),
        resolution: ResolutionTier::R512,
        seq: 2,
    });
    roundtrip(&Breed {
        parent_a: "aa".into(),
        parent_b: "bb".into(),
        seed: 99,
        breeder_pub: "cc".into(),
        resolution: ResolutionTier::R1024,
        seq: 4,
    });
    roundtrip(&Vote { sheep_id: "id".into(), seq: 1 });
    roundtrip(&Attestation {
        sheep_id: "id".into(),
        frame: 1,
        idx: 2,
        pass: 0,
        hash: "h".into(),
    });
    roundtrip(&Claim {
        block_id: "b".into(),
        expiry: 5,
        claimant: "c".into(),
        seq: 7,
    });
    roundtrip(&Heartbeat { block_id: "b".into() });
    roundtrip(&Coverage {
        sheep_id: "id".into(),
        frame: 3,
        idx: 4,
        pass: 2,
        hash: "h".into(),
    });
    roundtrip(&RepDelta {
        peer: "p".into(),
        rep: -5,
        banned: true,
    });
    roundtrip(&PieceUpload {
        sheep_id: "id".into(),
        frame: 1,
        idx: 2,
        pass: 1,
        hash: "h".into(),
        count: "9007199254740993".into(),
        hist_b64: "AAAA".into(),
    });
    roundtrip(&AssignReq {
        sheep_id: Some("id".into()),
        want: Some(8),
    });
    roundtrip(&AssignReq { sheep_id: None, want: None });
    roundtrip(&AssignResp {
        blocks: vec![AssignBlock {
            block_id: "b".into(),
            sheep_id: "id".into(),
            frame: 0,
            idx: 0,
        }],
        audits: vec![Coverage {
            sheep_id: "id".into(),
            frame: 1,
            idx: 1,
            pass: 0,
            hash: "h".into(),
        }],
    });
    roundtrip(&Seq::new(3, Vote { sheep_id: "id".into(), seq: 3 }));

    // Envelope round-trips (signed).
    let key = SigningKey::from_bytes(&[1u8; 32]);
    let mut e = Envelope::new("vote", "", 1, json!({ "sheep_id": "id", "seq": 1u64 }));
    e.sign(&key);
    roundtrip(&e);
    let eq = Equivocation::new(e.clone(), e.clone());
    roundtrip(&eq);
}
