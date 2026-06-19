//! Step-3 deliverable (ARCHITECTURE v3 §12-step-3, §10 reads): the **read-only
//! watch face**. Spawns the HTTP server in-process on a bound ephemeral port,
//! seeds a node's read-state with the genesis sheep + a few ingested pieces, and
//! asserts the watch-face contract:
//!
//! - `GET /api/flock` returns the sheep with sane coverage + a Cache-Control;
//! - `GET /api/sheep/:id` returns per-frame coverage from the accumulator;
//! - `GET /api/hall` returns the enshrined-sheep projection;
//! - the **tonemap → frames** step yields real RGBA frames (verified directly,
//!   independent of ffmpeg) and `GET /api/video/:id` exercises the encode path
//!   and serves a non-empty artifact with Cache-Control — OR degrades to 404 if
//!   ffmpeg is unavailable (the brief: don't make the test depend on ffmpeg, but
//!   DO verify the tonemap/serialization path).
//!
//! This drives the SAME `http::router` + `video` modules the live run loop uses;
//! the accumulator is seeded with honest pieces exactly as the loop ingests them.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use flame_core::chunked::{hist_hash_hex, render_batch, total_count};
use flame_core::genome::Genome;
use flame_core::rng::Rng;
use sheep_node::accumulator::Accumulator;
use sheep_node::http::{self, HallView, HttpState, ReadState, SheepView};
use sheep_node::hist::encode_accum;
use sheep_node::video;
use sheep_proto::msg::PieceUpload;

const EDGE: usize = 8; // small + fast (tonemap is resolution-agnostic here)
const SPP: u32 = 2_000;

fn test_genome() -> Genome {
    let mut rng = Rng::new(2);
    Genome::random(&mut rng, 3)
}

fn hex(b: &[u8]) -> String {
    let mut s = String::new();
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

/// An honest piece whose hash is the true hash of its bytes (mirrors the
/// accumulator's own test fixture + what `engine::render_active` emits).
fn honest_piece(g: &Genome, sheep: &[u8; 32], frame: u32, idx: u32, pass: u32) -> PieceUpload {
    // Mix `pass` into the seed exactly as the engine does (pass 0 = bare identity)
    // so a later pass is a DISTINCT histogram (raises density, not a duplicate).
    let seed_id = sheep_node::engine::pass_seed_id(sheep, pass);
    let accum = render_batch(g, &seed_id, frame, idx, EDGE, EDGE, 1, SPP, 128);
    PieceUpload {
        sheep_id: hex(sheep),
        frame,
        idx,
        pass,
        hash: hist_hash_hex(&accum),
        count: total_count(&accum).to_string(),
        hist_b64: encode_accum(&accum),
    }
}

/// Build a seeded HttpState: one sheep registered + a few honest pieces ingested
/// into a couple of frames, with a matching live ReadState entry + a Hall entry.
fn seeded_state(sheep_hex: &str, sheep: &[u8; 32], g: &Genome) -> (HttpState, tempdir::TempDir) {
    let dir = tempdir::TempDir::new();

    let mut acc = Accumulator::new();
    acc.register_sheep(sheep_hex, g.clone());
    // Frame 0: a few idx tiles + a 2nd pass for density. Frame 1: one tile.
    assert!(acc.ingest(&honest_piece(g, sheep, 0, 0, 0), EDGE));
    assert!(acc.ingest(&honest_piece(g, sheep, 0, 1, 0), EDGE));
    assert!(acc.ingest(&honest_piece(g, sheep, 0, 0, 1), EDGE));
    assert!(acc.ingest(&honest_piece(g, sheep, 1, 0, 0), EDGE));

    let mut live = HashMap::new();
    live.insert(
        sheep_hex.to_string(),
        SheepView {
            id: sheep_hex.to_string(),
            edge: 384,
            backing: 3,
            vitality: 2.5,
            coverage: 4,
            creator: "creatorpubhex".into(),
            parents: None,
            birth_ms: 1000,
            genome: serde_json::to_value(&g).expect("genome to value"),
        },
    );
    let hall = vec![HallView {
        id: "deadsheephex".into(),
        edge: 512,
        birth_ms: 0,
        death_ms: 30_000,
        lifespan_ms: 30_000,
        peak_backing: 20,
    }];
    let read = ReadState {
        self_pub: "selfpubhex".into(),
        live,
        hall,
        now_ms: 5000,
    };

    let st = HttpState {
        read: Arc::new(Mutex::new(read)),
        accum: Arc::new(Mutex::new(acc)),
        data_dir: dir.path().to_path_buf(),
        n_frames: 4,
        // Read-only watch-face test: no write channel into a run loop.
        cmd: None,
    };
    (st, dir)
}

/// Minimal scoped temp dir (no external dev-dep): unique path, removed on drop.
mod tempdir {
    use std::path::{Path, PathBuf};
    pub struct TempDir(PathBuf);
    impl TempDir {
        pub fn new() -> Self {
            let mut p = std::env::temp_dir();
            let uniq = format!(
                "sheep-http-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );
            p.push(uniq);
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
        pub fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

/// Spawn the watch face on an ephemeral port; return its base URL.
async fn spawn(st: HttpState) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = http::router(st);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch_face_serves_read_only_projections() {
    let g = test_genome();
    let sheep = [7u8; 32];
    let sheep_hex = hex(&sheep);

    // First, verify the pure tonemap→frames path independently of ffmpeg (the
    // brief: DO verify the tonemap/serialization path even if ffmpeg is flaky).
    {
        let (st, _dir) = seeded_state(&sheep_hex, &sheep, &g);
        let frames = video::tonemap_frames(&st.accum, &sheep_hex, st.n_frames)
            .expect("tonemap yields frames from the accumulator's merged histograms");
        assert_eq!(frames.edge, EDGE, "frame edge matches the ingested tile dims");
        assert!(
            frames.rgba.contains_key(&0) && frames.rgba.contains_key(&1),
            "both ingested frames tonemap"
        );
        for img in frames.rgba.values() {
            assert_eq!(img.len(), EDGE * EDGE * 4, "RGBA8 at edge x edge");
            assert!(img.iter().any(|&b| b != 0), "frame is not all-zero");
        }
    }

    let (st, _dir) = seeded_state(&sheep_hex, &sheep, &g);
    let base = spawn(st).await;
    let client = reqwest::Client::new();

    // ---- GET /api/flock --------------------------------------------------
    let r = client.get(format!("{base}/api/flock")).send().await.unwrap();
    assert!(r.status().is_success(), "flock 2xx");
    let cc = r
        .headers()
        .get("cache-control")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(cc.contains("max-age"), "flock carries Cache-Control: {cc}");
    let body: serde_json::Value = r.json().await.unwrap();
    let sheep_arr = body["sheep"].as_array().expect("sheep array");
    assert_eq!(sheep_arr.len(), 1, "one live sheep");
    let s0 = &sheep_arr[0];
    assert_eq!(s0["id"], sheep_hex);
    assert_eq!(s0["coverage"], 4, "sane coverage from the read-state");
    assert_eq!(s0["resolution"], 384);
    assert_eq!(s0["video"], format!("/api/video/{sheep_hex}"));
    assert_eq!(s0["creator"], "creatorpubhex");
    // §10 contribute: the genome is exposed so the browser can render tiles.
    assert!(
        s0["genome"]["transforms"].is_array(),
        "flock entry carries the renderable genome"
    );

    // ---- GET /api/sheep/:id ---------------------------------------------
    let r = client
        .get(format!("{base}/api/sheep/{sheep_hex}"))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "sheep detail 2xx");
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(body["alive"], true);
    let fc = body["frames_coverage"].as_array().expect("frames_coverage");
    assert_eq!(fc.len(), 4, "one entry per frame");
    // Frame 0 has 3 distinct tiles, frame 1 has 1, frames 2/3 have 0.
    assert_eq!(fc[0], 3, "frame 0 tile count");
    assert_eq!(fc[1], 1, "frame 1 tile count");
    assert_eq!(fc[2], 0);
    assert_eq!(body["accumulated_tiles"], 4);

    // unknown sheep → 404
    let r = client
        .get(format!("{base}/api/sheep/deadbeef"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::NOT_FOUND);

    // ---- GET /api/hall ---------------------------------------------------
    let r = client.get(format!("{base}/api/hall")).send().await.unwrap();
    assert!(r.status().is_success());
    let body: serde_json::Value = r.json().await.unwrap();
    let hall = body["sheep"].as_array().expect("hall array");
    assert_eq!(hall.len(), 1);
    assert_eq!(hall[0]["id"], "deadsheephex");
    assert_eq!(hall[0]["peak_backing"], 20);

    // ---- GET /api/video/:id ---------------------------------------------
    // Exercises the tonemap→ffmpeg encode + cache. ffmpeg may be absent in CI:
    // success → non-empty webm + Cache-Control; absence → graceful 404. Either
    // is acceptable; the encode path is invoked + the tonemap path is proven
    // above regardless.
    let r = client
        .get(format!("{base}/api/video/{sheep_hex}"))
        .send()
        .await
        .unwrap();
    if r.status().is_success() {
        let cc = r
            .headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert!(cc.contains("max-age"), "video carries Cache-Control: {cc}");
        let ct = r
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert_eq!(ct, "video/webm");
        let bytes = r.bytes().await.unwrap();
        assert!(!bytes.is_empty(), "video artifact is non-empty");
        println!("VIDEO OK: {} bytes", bytes.len());
    } else {
        assert_eq!(
            r.status(),
            reqwest::StatusCode::NOT_FOUND,
            "no-ffmpeg degrades to 404, not a crash"
        );
        println!("VIDEO SKIPPED (ffmpeg absent or no frames): graceful 404");
    }
}
