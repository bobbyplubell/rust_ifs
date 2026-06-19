//! Canonicalization — the byte-exact reproduction of
//! `coordinator/src/auth.rs::canonical_message` / `web/js/api.js::canonicalize`.
//!
//! Rule (identical at every nesting depth):
//!   - objects: keys sorted lexicographically, emitted `{"k":v,...}` with no
//!     whitespace; the `sig` key is stripped from the *top-level* object only
//!     (the auth oracle does `obj.remove("sig")` on the root, then recurses);
//!   - arrays: order preserved, elements canonicalized in place;
//!   - scalars: standard compact JSON (`serde_json::to_string`).
//!
//! `serde_json::Map` without the `preserve_order` feature is a `BTreeMap`
//! (already sorted), but we rebuild explicitly — exactly as `auth.rs` does — so
//! the contract is legible and does not silently depend on a feature flag. The
//! `float_roundtrip` feature keeps float parse lossless so numbers round-trip
//! byte-identically.

use serde_json::Value;

/// Recursively rebuild a `Value` so every object's keys are in sorted order.
/// Mirrors `auth.rs::sort_value` exactly.
pub fn sort_value(v: Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut sorted = serde_json::Map::new();
            let mut entries: Vec<(String, Value)> = map.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            for (k, val) in entries {
                sorted.insert(k, sort_value(val));
            }
            Value::Object(sorted)
        }
        Value::Array(arr) => Value::Array(arr.into_iter().map(sort_value).collect()),
        other => other,
    }
}

/// Canonicalize an arbitrary JSON value: strip the top-level `sig`, sort keys
/// recursively, serialize compact. This is the byte-exact reproduction of
/// `auth.rs::canonical_message`.
pub fn canonicalize_value(value: &Value) -> String {
    let mut clone = value.clone();
    if let Some(obj) = clone.as_object_mut() {
        obj.remove("sig");
    }
    let sorted = sort_value(clone);
    // serde_json::to_string on a Value never fails.
    serde_json::to_string(&sorted).expect("canonicalize: Value serialization cannot fail")
}
