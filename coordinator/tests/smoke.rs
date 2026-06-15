//! End-to-end smoke test: boot the coordinator binary, then drive the core
//! loop over real HTTP — flock → assign → render the assigned tile with
//! flame-core (exactly as the WASM client would) → submit → verify it's
//! accepted and credited.
//!
//! Run with: `cargo test -p coordinator --test smoke -- --nocapture`

use std::process::{Child, Command};
use std::time::Duration;

use ed25519_dalek::{Signer, SigningKey};
use serde_json::{json, Value};

/// A throwaway temp data dir + server child that cleans up on drop.
struct Server {
    child: Child,
    base: String,
    _dir: tempdir::TempDir,
}

mod tempdir {
    // Minimal temp-dir helper (no extra dep): a unique dir under std::env::temp_dir.
    use std::path::PathBuf;
    pub struct TempDir(pub PathBuf);
    impl TempDir {
        pub fn new(tag: &str) -> std::io::Result<Self> {
            let mut p = std::env::temp_dir();
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            p.push(format!("coord-{tag}-{nanos}"));
            std::fs::create_dir_all(&p)?;
            Ok(TempDir(p))
        }
        pub fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

impl Server {
    fn boot() -> Server {
        let dir = tempdir::TempDir::new("smoke").unwrap();
        // Pick a port; 0 would be ideal but the binary binds a fixed addr, so
        // use a high fixed port and hope it's free in CI/local.
        let port = 18421;
        let base = format!("http://127.0.0.1:{port}");

        // The genomes dir relative to the crate. Falls back to random genomes if
        // not found, so the test still runs.
        let genomes = concat!(env!("CARGO_MANIFEST_DIR"), "/../web/genomes");

        let child = Command::new(env!("CARGO_BIN_EXE_coordinator"))
            .env("BIND", format!("127.0.0.1:{port}"))
            .env("DATA_DIR", dir.path())
            .env("GENOMES_DIR", genomes)
            .env("GEN_MS", "86400000")
            .env("RUST_LOG", "coordinator=warn")
            .spawn()
            .expect("spawn coordinator");

        let s = Server { child, base, _dir: dir };
        s.wait_ready();
        s
    }

    fn wait_ready(&self) {
        let client = reqwest::blocking::Client::new();
        for _ in 0..50 {
            if client
                .get(format!("{}/health", self.base))
                .timeout(Duration::from_millis(300))
                .send()
                .map(|r| r.status().is_success())
                .unwrap_or(false)
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        panic!("coordinator did not become ready");
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Sign a request body (object with `pub` + `nonce`) the way the client does:
/// canonical message = body minus `sig`, keys sorted, compact.
fn sign(sk: &SigningKey, mut body: Value) -> Value {
    let pub_hex = hex::encode(sk.verifying_key().to_bytes());
    body.as_object_mut().unwrap().insert("pub".into(), json!(pub_hex));
    // Canonical message: re-serialize through Value (BTreeMap = sorted), no sig.
    let mut for_sig = body.clone();
    for_sig.as_object_mut().unwrap().remove("sig");
    let msg = serde_json::to_string(&for_sig).unwrap();
    let sig = sk.sign(msg.as_bytes());
    body.as_object_mut()
        .unwrap()
        .insert("sig".into(), json!(hex::encode(sig.to_bytes())));
    body
}

#[test]
fn assign_submit_round_trip() {
    let server = Server::boot();
    let client = reqwest::blocking::Client::new();
    let sk = SigningKey::from_bytes(&[42u8; 32]);

    // 1. GET /api/flock — there should be a seeded flock.
    let flock: Value = client
        .get(format!("{}/api/flock", server.base))
        .send()
        .unwrap()
        .json()
        .unwrap();
    let sheep = flock["sheep"].as_array().expect("sheep array");
    assert!(!sheep.is_empty(), "flock should be seeded");

    // 2. POST /api/assign — get a bundle of work units.
    let assign_body = sign(&sk, json!({ "nonce": 1u64 }));
    let assign: Value = client
        .post(format!("{}/api/assign", server.base))
        .json(&assign_body)
        .send()
        .unwrap()
        .json()
        .unwrap();
    let units = assign["units"].as_array().expect("units array");
    assert!(!units.is_empty(), "assign should hand out units: {assign}");

    // 3. Render each unit with flame-core (== what the WASM client does), build
    //    Result objects.
    let mut results = Vec::new();
    for u in units {
        let genome_json = u["genomeJson"].as_str().unwrap();
        let sheep_id = u["sheepId"].as_str().unwrap();
        let frame = u["frame"].as_u64().unwrap() as u32;
        let idx = u["idx"].as_u64().unwrap() as u32;
        let w = u["w"].as_u64().unwrap() as usize;
        let h = u["h"].as_u64().unwrap() as usize;
        let ss = u["ss"].as_u64().unwrap() as usize;
        let spp = u["spp"].as_u64().unwrap() as u32;
        let nf = u["nFrames"].as_u64().unwrap() as u32;

        let genome: flame_core::genome::Genome = serde_json::from_str(genome_json).unwrap();
        let sid = flame_core::canonical::sheep_id(&genome);
        // sanity: assigned sheepId must equal the genome's canonical id.
        assert_eq!(hex::encode(sid), sheep_id);

        let accum =
            flame_core::chunked::render_batch(&genome, &sid, frame, idx, w, h, ss, spp, nf);
        let hash = flame_core::chunked::hist_hash_hex(&accum);
        let count = flame_core::chunked::total_count(&accum);
        let cells: Vec<u64> = accum.data.iter().flatten().copied().collect();
        let hist = encode_hist(&cells);

        results.push(json!({
            "sheepId": sheep_id,
            "frame": frame,
            "idx": idx,
            "hash": hash,
            "count": count.to_string(),
            "hist": hist,
        }));
    }

    // 4. POST /api/submit — verify acceptance.
    let submit_body = sign(&sk, json!({ "nonce": 2u64, "results": results }));
    let resp = client
        .post(format!("{}/api/submit", server.base))
        .json(&submit_body)
        .send()
        .unwrap();
    let status = resp.status();
    let text = resp.text().unwrap();
    let submit: Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("submit body not JSON (status {status}): {e}: body={text:?}"));
    assert!(status.is_success(), "submit failed: {submit}");
    let accepted = submit["accepted"].as_u64().unwrap();
    assert_eq!(accepted as usize, units.len(), "all units should be accepted: {submit}");
    assert_eq!(submit["rejected"].as_u64().unwrap(), 0);

    // 5. GET /api/me — tiles should be credited.
    let pub_hex = hex::encode(sk.verifying_key().to_bytes());
    let me: Value = client
        .get(format!("{}/api/me?pub={pub_hex}", server.base))
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(me["tiles"].as_u64().unwrap() as usize, units.len());

    // 6. A tampered submit (wrong hash) must be rejected/banned.
    let sk2 = SigningKey::from_bytes(&[99u8; 32]);
    let assign2_body = sign(&sk2, json!({ "nonce": 1u64 }));
    let assign2: Value = client
        .post(format!("{}/api/assign", server.base))
        .json(&assign2_body)
        .send()
        .unwrap()
        .json()
        .unwrap();
    let u = &assign2["units"].as_array().unwrap()[0];
    let bogus = json!([{
        "sheepId": u["sheepId"],
        "frame": u["frame"],
        "idx": u["idx"],
        "hash": "00".repeat(32),
        "count": "1",
        "hist": encode_hist(&vec![0u64; (384*384*4) as usize]),
    }]);
    let bad_submit = sign(&sk2, json!({ "nonce": 2u64, "results": bogus }));
    let resp = client
        .post(format!("{}/api/submit", server.base))
        .json(&bad_submit)
        .send()
        .unwrap();
    assert_eq!(resp.status(), 403, "fraud should be forbidden + banned");

    println!("SMOKE OK: {} tiles accepted, fraud banned", accepted);
}

/// Same encoding as `histio::encode_hist` (base64 + zlib deflate of LE u64s).
fn encode_hist(cells: &[u64]) -> String {
    use base64::Engine;
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write;

    let mut raw = Vec::with_capacity(cells.len() * 8);
    for c in cells {
        raw.extend_from_slice(&c.to_le_bytes());
    }
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(&raw).unwrap();
    let compressed = enc.finish().unwrap();
    base64::engine::general_purpose::STANDARD.encode(compressed)
}
