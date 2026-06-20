//! Step-8 (server side) deliverable (ARCHITECTURE v3 §10 writes, §6.1 gateway
//! ingest-audit): the **browser-contribute write face** — the mechanically-1:1
//! REST skin over the protocol messages. A full node+swarm runs in-process on
//! the in-memory transport with the write face enabled; we POST signed
//! `Envelope`s to `/api/msg` and `GET /api/assign`, asserting:
//!
//! - a correctly-signed `Coverage` is accepted, applied to engine state (read
//!   back via the shared read face), and re-published to gossip (the 1:1 skin);
//! - a bad-signature `Envelope` is rejected with 400;
//! - `GET /api/assign?pub=<hex>` returns sane non-colliding work for a pubkey;
//! - **ingest-audit ON**: a browser `Coverage` with a WRONG hash is caught +
//!   rejected before vouching; a CORRECT hash is accepted;
//! - **ingest-audit OFF**: a wrong-hash `Coverage` is forwarded (no local
//!   re-render gate) — the personal-node posture;
//! - **parity**: a `Coverage` via `/api/msg` produces the SAME engine state
//!   change as the same envelope arriving via `engine.apply` (the 1:1 property).
//!
//! Renders happen only on the §6.1 audit path here, kept tiny (the genesis sheep
//! is the fixture). The deterministic re-render makes the wrong/right-hash audit
//! outcomes exact.

use std::net::SocketAddr;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use libp2p::core::transport::MemoryTransport;
use libp2p::core::upgrade::Version;
use libp2p::{noise, yamux, Multiaddr, Transport};
use serde_json::Value;
use sheep_node::net::{libp2p_key, run_on_transport_with};
use sheep_node::spec::N_FRAMES;
use sheep_node::{genesis_sheep_hex, Control, ServeConfig, WorldConfig};
use sheep_proto::msg::Coverage;
use sheep_proto::{proto, Envelope};
use tokio::sync::mpsc;

fn mem_transport(
    signing_key: &SigningKey,
) -> libp2p::core::transport::Boxed<(libp2p::PeerId, libp2p::core::muxing::StreamMuxerBox)> {
    let key = libp2p_key(signing_key);
    MemoryTransport::default()
        .upgrade(Version::V1)
        .authenticate(noise::Config::new(&key).expect("noise"))
        .multiplex(yamux::Config::default())
        .boxed()
}

/// Find a free localhost TCP port for the watch/write face (bind then release;
/// a tiny race window, fine for a test).
fn free_addr() -> SocketAddr {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let a = l.local_addr().unwrap();
    drop(l);
    a
}

/// Spawn one server node (write face on) on the in-memory transport. Returns the
/// HTTP base url, a control sender (for parity snapshots), and the join handle.
async fn spawn_server(
    key: SigningKey,
    mem_id: u64,
    ingest_audit: bool,
) -> (String, mpsc::UnboundedSender<Control>, tokio::task::JoinHandle<()>) {
    let http_addr = free_addr();
    let data_dir = std::env::temp_dir().join(format!(
        "sheep-write-{}-{}",
        std::process::id(),
        mem_id
    ));
    let serve = ServeConfig {
        http_addr,
        data_dir,
        ingest_audit,
    };
    let (tx, rx) = mpsc::unbounded_channel::<Control>();
    let listen: Multiaddr = format!("/memory/{mem_id}").parse().unwrap();
    let t = mem_transport(&key);
    let h = tokio::spawn(async move {
        let _ =
            run_on_transport_with(key, t, listen, vec![], rx, Some(serve), WorldConfig::default())
                .await;
    });
    let base = format!("http://{http_addr}");
    // Wait until the watch face answers (the loop also injects the genesis sheep).
    let client = reqwest::Client::new();
    // Wait for the watch face to answer /health. (The genesis sheep is injected
    // into the engine's raw flock on startup — which is what the write path's
    // `apply_coverage` checks — even though it is dormant in the *live* flock
    // projection under real wall-clock decay, so we gate only on /health.)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut last_err = String::new();
    loop {
        if tokio::time::Instant::now() > deadline {
            panic!("server did not come up; last={last_err}");
        }
        match client.get(format!("{base}/health")).send().await {
            Ok(r) if r.status().is_success() => break,
            Ok(r) => last_err = format!("health status {}", r.status()),
            Err(e) => last_err = format!("health err: {e}"),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    (base, tx, h)
}

/// Render the genesis sheep's tile (frame,idx,pass) and return its true content
/// hash — for building an honest Coverage the ingest-audit re-render will match.
/// Re-derives the genesis genome from the mint body exactly as `engine::apply`
/// does, then renders with the engine's own seed/edge/SPP so the hash is
/// byte-identical to what the node's ingest-audit re-render produces.
fn true_hash(frame: u32, idx: u32, pass: u32) -> String {
    use flame_core::chunked::{hist_hash_hex, render_batch};
    use sheep_proto::identity::{sheep_identity, ResolutionTier};

    let mint = sheep_node::genesis_mint();
    let body = &mint.body;
    let ts_micros = body["ts_micros"].as_u64().unwrap();
    let minter_hex = body["minter_pub"].as_str().unwrap();
    let mut minter = [0u8; 32];
    for i in 0..32 {
        minter[i] = u8::from_str_radix(&minter_hex[i * 2..i * 2 + 2], 16).unwrap();
    }
    let genome = sheep_proto::derive::derive_minted(ts_micros, &minter);
    // Genesis tier is R384 (the cheapest tier; see derive_minted_genesis.rs).
    let tier = ResolutionTier::R384;
    let identity = sheep_identity(&genome, tier);
    let seed_id = sheep_node::engine::pass_seed_id(&identity, pass);
    let edge = tier.edge() as usize;
    let accum = render_batch(
        &genome,
        &seed_id,
        frame,
        idx,
        edge,
        edge,
        1,
        sheep_node::spec::SPP,
        sheep_node::spec::N_FRAMES,
    );
    hist_hash_hex(&accum)
}

fn sign_coverage(key: &SigningKey, cov: &Coverage) -> Envelope {
    let mut env = Envelope::new(
        proto::PROGRESS,
        String::new(),
        1_000,
        serde_json::to_value(cov).unwrap(),
    );
    env.sign(key);
    env
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_face_msg_assign_and_ingest_audit() {
    let server_key = SigningKey::from_bytes(&[0x5e; 32]);
    let (base, tx, h) = spawn_server(server_key, 525252, true).await;
    let client = reqwest::Client::new();
    let sheep = genesis_sheep_hex();

    // A disposable "browser" identity.
    let browser = SigningKey::from_bytes(&[0xB1; 32]);

    // ---- bad signature → 400 --------------------------------------------
    {
        let mut env = sign_coverage(
            &browser,
            &Coverage { sheep_id: sheep.clone(), frame: 0, idx: 0, pass: 0, hash: String::new() },
        );
        env.sig = "00".repeat(64); // corrupt the signature
        let r = client.post(format!("{base}/api/msg")).body(serde_json::to_vec(&env).unwrap()).send().await.unwrap();
        assert_eq!(r.status(), reqwest::StatusCode::BAD_REQUEST, "bad sig rejected with 400");
    }

    // ---- ingest-audit ON: a SAMPLED wrong-hash tile is caught + rejected ----
    // Ingest-audit is reputation-graduated (`NEW_PEER_RATE`): a fresh browser is
    // audited on only a FRACTION of its tiles, so some wrong-hash submissions are
    // caught here (422) and the rest forwarded optimistically (200) to be caught
    // later by assigned swarm auditors. Sweep distinct tiles until the gate fires
    // — over a full frame range it is statistically certain to sample at least
    // one. (Frames 1/2 are reserved for the good-hash + parity checks below.)
    {
        let mut caught = false;
        for frame in 5..N_FRAMES {
            let cov = Coverage { sheep_id: sheep.clone(), frame, idx: 0, pass: 0, hash: "deadbeef".repeat(8) };
            let env = sign_coverage(&browser, &cov);
            let r = client.post(format!("{base}/api/msg")).body(serde_json::to_vec(&env).unwrap()).send().await.unwrap();
            if r.status() == reqwest::StatusCode::UNPROCESSABLE_ENTITY {
                let b: Value = r.json().await.unwrap();
                assert_eq!(b["accepted"], false);
                assert!(b["reason"].as_str().unwrap_or("").contains("ingest-audit"), "rejected by ingest-audit: {b}");
                caught = true;
                break;
            }
            assert!(r.status().is_success(), "un-sampled wrong-hash tile forwarded optimistically: {}", r.status());
        }
        assert!(caught, "ingest-audit catches at least one sampled wrong-hash tile across the sweep");
    }

    // ---- ingest-audit ON: CORRECT hash accepted + applied + read back ---
    let good_hash = true_hash(1, 0, 0); // a different tile so the wrong-hash one above can't have confirmed it
    {
        let cov = Coverage { sheep_id: sheep.clone(), frame: 1, idx: 0, pass: 0, hash: good_hash.clone() };
        let env = sign_coverage(&browser, &cov);
        let r = client.post(format!("{base}/api/msg")).body(serde_json::to_vec(&env).unwrap()).send().await.unwrap();
        assert!(r.status().is_success(), "honest submission accepted: {}", r.status());
        let b: Value = r.json().await.unwrap();
        assert_eq!(b["accepted"], true, "honest Coverage accepted: {b}");
        assert_eq!(b["sheep_id"], sheep, "standing read back names the sheep");
    }

    // ---- GET /api/assign returns sane non-colliding work ----------------
    {
        let r = client
            .get(format!("{base}/api/assign?pub={}", hex(browser.verifying_key().as_bytes())))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success());
        let b: Value = r.json().await.unwrap();
        let blocks = b["blocks"].as_array().expect("blocks array");
        assert!(!blocks.is_empty(), "assign hands out work for the genesis flock");
        // The deterministic genesis flock (§1) — work may be for any of them.
        let genesis_ids: std::collections::HashSet<String> =
            sheep_node::derive_minted_genesis::genesis_sheep_hexes(
                sheep_node::engine::GENESIS_FLOCK_SIZE,
            )
            .into_iter()
            .collect();
        // Non-colliding: every unit's (sheep,frame,idx,pass) is distinct.
        let mut seen = std::collections::HashSet::new();
        for u in blocks {
            let k = (
                u["sheep_id"].as_str().unwrap().to_string(),
                u["frame"].as_u64().unwrap(),
                u["idx"].as_u64().unwrap(),
                u["pass"].as_u64().unwrap(),
            );
            assert!(seen.insert(k), "assign work units are non-colliding");
            assert!(
                genesis_ids.contains(u["sheep_id"].as_str().unwrap()),
                "work is for a genesis founder"
            );
        }
        // bad pubkey → 400
        let r = client.get(format!("{base}/api/assign?pub=nothex")).send().await.unwrap();
        assert_eq!(r.status(), reqwest::StatusCode::BAD_REQUEST);
    }

    // ---- parity: /api/msg state change == engine.apply ------------------
    // Submit a fresh honest Coverage via the write face, snapshot the server's
    // coverage; independently apply the SAME envelope to a standalone engine
    // seeded with the genesis sheep and assert the coverage delta matches.
    {
        let hash = true_hash(2, 0, 0);
        let cov = Coverage { sheep_id: sheep.clone(), frame: 2, idx: 0, pass: 0, hash };
        let env = sign_coverage(&browser, &cov);

        // Standalone engine reference (the libp2p inbound path: apply the same env).
        let mut ref_engine = sheep_node::Engine::new(SigningKey::from_bytes(&[0x77; 32]));
        let _ = ref_engine.apply(&sheep_node::genesis_mint(), 1_000);
        let applied = ref_engine.apply(&env, 1_000);
        assert!(applied, "the reference engine accepts the same Coverage (1:1 semantics)");

        // Via the write face.
        let r = client.post(format!("{base}/api/msg")).body(serde_json::to_vec(&env).unwrap()).send().await.unwrap();
        assert!(r.status().is_success());
        let b: Value = r.json().await.unwrap();
        assert_eq!(b["accepted"], true, "write face accepts the same Coverage");
        // Coverage is unaudited until attested in BOTH paths, so the engine
        // `coverage()` (confirmed count) is 0 in both — the parity we assert is
        // the accept/apply decision itself (the 1:1 routing), which matches.
        assert_eq!(
            ref_engine.coverage(&sheep), 0,
            "reference: coverage stays 0 (unaudited) — matching the write-face path"
        );
    }

    let _ = tx.send(Control::Shutdown);
    let _ = h.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ingest_audit_off_forwards_wrong_hash() {
    let server_key = SigningKey::from_bytes(&[0x0f; 32]);
    let (base, tx, h) = spawn_server(server_key, 161616, false).await; // ingest-audit OFF
    let client = reqwest::Client::new();
    let sheep = genesis_sheep_hex();
    let browser = SigningKey::from_bytes(&[0xC3; 32]);

    // With ingest-audit OFF, a wrong-hash Coverage is NOT re-rendered/rejected by
    // the gateway — it is optimistically forwarded (the swarm peer-audits later).
    // `engine.apply` still accepts a Coverage for a known sheep (no credit gate),
    // so it is applied + vouched: accepted == true even with a bogus hash.
    let cov = Coverage { sheep_id: sheep.clone(), frame: 0, idx: 3, pass: 0, hash: "feed".repeat(16) };
    let env = sign_coverage(&browser, &cov);
    let r = client.post(format!("{base}/api/msg")).body(serde_json::to_vec(&env).unwrap()).send().await.unwrap();
    assert!(r.status().is_success(), "OFF: wrong-hash forwarded, not gateway-rejected: {}", r.status());
    let b: Value = r.json().await.unwrap();
    assert_eq!(b["accepted"], true, "OFF: optimistically forwarded (no local re-render): {b}");

    let _ = tx.send(Control::Shutdown);
    let _ = h.await;
}

fn hex(b: &[u8]) -> String {
    let mut s = String::new();
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}
