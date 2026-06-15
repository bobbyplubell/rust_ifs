//! Ed25519 request authentication.
//!
//! Canonical message (per API.md) = the request's JSON body with `sig` omitted,
//! keys sorted, compact (no whitespace), UTF-8. The client signs those exact
//! bytes; we reconstruct them, verify the signature against `pub`, and reject
//! replayed nonces (last-seen-per-pubkey, monotonic).

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde_json::Value;

use crate::error::{ApiError, ApiResult};

/// The auth envelope every mutating request carries.
pub struct Auth {
    pub pub_hex: String,
    pub nonce: u64,
}

/// Verify a signed request body.
///
/// `body` is the full parsed JSON object (including `pub`, `nonce`, `sig`).
/// Returns the verified `Auth` on success. Does NOT touch nonce replay state —
/// the caller checks/commits the nonce against the DB so it's transactional.
pub fn verify(body: &Value) -> ApiResult<Auth> {
    let obj = body
        .as_object()
        .ok_or_else(|| ApiError::bad("request body must be a JSON object"))?;

    let pub_hex = obj
        .get("pub")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::bad("missing `pub`"))?
        .to_string();
    let sig_hex = obj
        .get("sig")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::bad("missing `sig`"))?;
    let nonce = obj
        .get("nonce")
        .and_then(Value::as_u64)
        .ok_or_else(|| ApiError::bad("missing or non-integer `nonce`"))?;

    // Decode key + signature.
    let pub_bytes: [u8; 32] = hex::decode(&pub_hex)
        .ok()
        .and_then(|v| v.try_into().ok())
        .ok_or_else(|| ApiError::bad("`pub` must be 32-byte hex"))?;
    let sig_bytes: [u8; 64] = hex::decode(sig_hex)
        .ok()
        .and_then(|v| v.try_into().ok())
        .ok_or_else(|| ApiError::bad("`sig` must be 64-byte hex"))?;

    let key = VerifyingKey::from_bytes(&pub_bytes)
        .map_err(|e| ApiError::bad(format!("invalid public key: {e}")))?;
    let sig = Signature::from_bytes(&sig_bytes);

    let msg = canonical_message(body)?;
    key.verify(msg.as_bytes(), &sig)
        .map_err(|_| ApiError::unauthorized("signature verification failed"))?;

    Ok(Auth { pub_hex, nonce })
}

/// Build the canonical signed message: the body minus `sig`, keys sorted
/// recursively, serialized compact. `serde_json::Value`'s `Map` is a BTreeMap
/// when the `preserve_order` feature is OFF (our default), so keys serialize
/// sorted automatically — but we strip `sig` and re-serialize to be explicit
/// and to match the client's "keys sorted, compact" rule at every depth.
pub fn canonical_message(body: &Value) -> ApiResult<String> {
    let mut clone = body.clone();
    if let Some(obj) = clone.as_object_mut() {
        obj.remove("sig");
    }
    let sorted = sort_value(clone);
    serde_json::to_string(&sorted).map_err(|e| ApiError::internal(format!("canonicalize: {e}")))
}

/// Recursively rebuild a Value so every object's keys are in sorted order.
/// `serde_json::Map` without `preserve_order` is already a BTreeMap (sorted),
/// so this is mainly belt-and-suspenders / explicitness.
fn sort_value(v: Value) -> Value {
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

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use serde_json::json;

    #[test]
    fn sign_and_verify_roundtrip() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let pub_hex = hex::encode(sk.verifying_key().to_bytes());

        // Build a body, sign the canonical message, then verify.
        let mut body = json!({ "pub": pub_hex, "nonce": 42, "sheepId": "abc" });
        let msg = canonical_message(&body).unwrap();
        let sig = sk.sign(msg.as_bytes());
        body.as_object_mut()
            .unwrap()
            .insert("sig".into(), json!(hex::encode(sig.to_bytes())));

        let auth = verify(&body).unwrap();
        assert_eq!(auth.nonce, 42);
    }

    #[test]
    fn tamper_is_rejected() {
        let sk = SigningKey::from_bytes(&[9u8; 32]);
        let pub_hex = hex::encode(sk.verifying_key().to_bytes());
        let mut body = json!({ "pub": pub_hex, "nonce": 1, "sheepId": "x" });
        let msg = canonical_message(&body).unwrap();
        let sig = sk.sign(msg.as_bytes());
        body.as_object_mut()
            .unwrap()
            .insert("sig".into(), json!(hex::encode(sig.to_bytes())));
        // Tamper after signing.
        body.as_object_mut()
            .unwrap()
            .insert("sheepId".into(), json!("y"));
        assert!(verify(&body).is_err());
    }

    #[test]
    fn key_order_does_not_matter() {
        // The canonical message must be identical regardless of input key order.
        let a = json!({ "nonce": 1, "pub": "aa", "sheepId": "z" });
        let b = json!({ "sheepId": "z", "pub": "aa", "nonce": 1 });
        assert_eq!(
            canonical_message(&a).unwrap(),
            canonical_message(&b).unwrap()
        );
    }
}
