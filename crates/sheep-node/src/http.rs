//! The **serve** capability (ARCHITECTURE v3 §1.1, §10) — the node's read-only
//! "watch face." An axum HTTP server, spawned alongside the libp2p swarm in the
//! run loop, that serves *cacheable projections of the same state types the
//! gossip layer carries*: the live flock snapshot, per-sheep detail, the Hall of
//! Fame, and a tonemapped loop video. **Read-only** — there are NO write /
//! contribute endpoints here (that's a separate later task; §10 writes half).
//!
//! It is a read cache, not a parallel protocol (§10): every value it returns is
//! derived from the engine's state (`live_flock`, `coverage`, `hall`, §2.4
//! attribution/lineage) or the accumulator's merged histograms (the same
//! `(sheep, frame)` CRDT sums the gossip-fed pieces build), so it cannot diverge
//! from what the swarm computes.
//!
//! Concurrency: the swarm's event loop owns the [`Engine`] (and renders on a
//! blocking thread), so HTTP handlers never touch it directly. Instead the loop
//! publishes a [`ReadState`] snapshot into a shared `Arc<Mutex<ReadState>>` each
//! time it holds the engine, and the [`Accumulator`] lives behind its own
//! `Arc<Mutex<_>>` shared with the loop. Handlers read those — never the engine
//! — so a long render never blocks the watch face.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::extract::{Path as AxPath, Query, State};
use axum::http::{header, HeaderValue, Method, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

use crate::accumulator::Accumulator;
use crate::net::{AssignResult, Control, InjectResult};
use crate::video;
use sheep_proto::Envelope;

/// Cache-Control max-age (seconds) on the watch-face responses. Snapshots are
/// monotonic projections, so a short cache is safe + offloads repeated polls.
const SNAPSHOT_MAX_AGE: u64 = 5;
/// Video is heavier and regenerated only on a coverage step, so it caches longer.
const VIDEO_MAX_AGE: u64 = 60;

/// One sheep's read-projection — serialized straight from engine state types
/// (`FlockEntry` + log-derived coverage/backing/vitality, §2.2/§2.4). The HTTP
/// layer only ever reads these, never the live engine.
#[derive(Debug, Clone)]
pub struct SheepView {
    pub id: String,
    pub edge: u32,
    pub backing: u64,
    pub vitality: f64,
    pub coverage: u64,
    /// §2.4 attribution: the key that signed the birth (lowercase hex).
    pub creator: String,
    /// §2.4 lineage: `(parent_a, parent_b)` for a bred sheep, else `None`.
    pub parents: Option<(String, String)>,
    pub birth_ms: u64,
    /// The sheep's flame genome as a JSON value (§10 contribute): the browser
    /// worker needs the full genome to render this sheep's tiles in WASM. It is
    /// a deterministic projection of engine state (`FlockEntry::genome`), so it
    /// stays a read cache, never a second source of truth.
    pub genome: Value,
}

/// One enshrined sheep (`engine.hall()` projection).
#[derive(Debug, Clone)]
pub struct HallView {
    pub id: String,
    pub edge: u32,
    pub birth_ms: u64,
    pub death_ms: u64,
    pub lifespan_ms: u64,
    pub peak_backing: u64,
}

/// The read snapshot the event loop publishes for the HTTP face. Refreshed
/// whenever the loop holds the engine; handlers read the latest. Monotonic
/// progress means a slightly-stale read is always safe.
#[derive(Debug, Clone, Default)]
pub struct ReadState {
    pub self_pub: String,
    /// Live flock (vitality > 0), keyed by sheep id hex.
    pub live: HashMap<String, SheepView>,
    /// The Hall of Fame (enshrined dead sheep).
    pub hall: Vec<HallView>,
    /// Wall clock (ms) at which this snapshot was taken — for `vitality` recompute
    /// staleness display only; the value is already baked into each `SheepView`.
    pub now_ms: u64,
}

/// Shared HTTP server state: the latest engine read-snapshot + the live
/// accumulator (the merged per-`(sheep, frame)` histograms video tonemaps from).
#[derive(Clone)]
pub struct HttpState {
    pub read: Arc<Mutex<ReadState>>,
    pub accum: Arc<Mutex<Accumulator>>,
    /// On-disk dir for cached encoded videos (regenerable, §5).
    pub data_dir: PathBuf,
    /// Animation loop length (frames) — `spec::N_FRAMES`.
    pub n_frames: u32,
    /// §10 writes — the command channel into the run loop. `Some` enables the
    /// write face (`POST /api/msg`, `GET /api/assign`); `None` (e.g. the read-
    /// only watch-face test) leaves the node read-only. A write handler hands a
    /// verified [`Envelope`] (or an assign request) to the loop, which routes it
    /// through the SAME apply+gossip path the libp2p inbound path uses (§10 1:1).
    #[allow(clippy::type_complexity)]
    pub cmd: Option<mpsc::UnboundedSender<Control>>,
}

/// Build the watch-face router (§10 reads half) + the §10 write skin
/// (`POST /api/msg`, `GET /api/assign`) when [`HttpState::cmd`] is set.
pub fn router(state: HttpState) -> Router {
    Router::new()
        .route("/api/flock", get(get_flock))
        .route("/api/sheep/:id", get(get_sheep))
        .route("/api/video/:id", get(get_video))
        .route("/api/hall", get(get_hall))
        // §10 writes (contribute) — the 1:1 REST skin over the protocol.
        .route("/api/msg", post(post_msg))
        .route("/api/assign", get(get_assign))
        .route("/health", get(get_health))
        .layer(middleware::from_fn(cors))
        .with_state(state)
}

/// Permissive CORS for the v3 browser client. The watch face is a public,
/// read-only API and the write face (`POST /api/msg`) is signed end-to-end
/// (Ed25519 over the canonical envelope), so a forged origin cannot inject a
/// valid envelope — auth is the signature, not the origin. The deployed browser
/// client is served from a DIFFERENT origin (e.g. GitHub Pages) than the node,
/// so `Access-Control-Allow-Origin: *` is required for any browser to reach it.
/// We also short-circuit the `OPTIONS` preflight that a cross-origin
/// `POST .../api/msg` (a non-simple request) triggers.
async fn cors(req: Request<axum::body::Body>, next: Next) -> Response {
    let is_preflight = req.method() == Method::OPTIONS;
    let mut res = if is_preflight {
        StatusCode::NO_CONTENT.into_response()
    } else {
        next.run(req).await
    };
    let h = res.headers_mut();
    h.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    h.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    h.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("content-type, accept"),
    );
    h.insert(
        header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static("86400"),
    );
    res
}

/// Bind + serve the watch face on `addr` until the process ends. Returns once
/// the bind fails (so the loop can log it) or the server stops.
pub async fn serve(addr: SocketAddr, state: HttpState) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let app = router(state);
    axum::serve(listener, app).await
}

/// Cache-Control header value for a snapshot response.
fn snapshot_cache() -> (header::HeaderName, String) {
    (
        header::CACHE_CONTROL,
        format!("public, max-age={SNAPSHOT_MAX_AGE}"),
    )
}

// ---- GET /api/flock ---------------------------------------------------------

/// The live flock snapshot (§2.3 `live_flock`): every sheep alive at the last
/// tick, with its resolution, backing/vitality, coverage, lineage/creator, and
/// the video url. Serialized from the same `FlockEntry` + log-derived state the
/// gossip layer carries (§10 reads).
async fn get_flock(State(st): State<HttpState>) -> Response {
    let read = st.read.lock().unwrap();
    let mut sheep: Vec<Value> = read.live.values().map(sheep_json).collect();
    // Stable order: most-covered first (mirrors the coordinator's flock order).
    sheep.sort_by(|a, b| {
        b["coverage"]
            .as_u64()
            .cmp(&a["coverage"].as_u64())
            .then_with(|| a["id"].as_str().cmp(&b["id"].as_str()))
    });
    let body = json!({
        "self": read.self_pub,
        "now_ms": read.now_ms,
        "sheep": sheep,
    });
    ([snapshot_cache()], Json(body)).into_response()
}

/// JSON for one sheep, shared by `/api/flock` and `/api/sheep/:id`.
fn sheep_json(s: &SheepView) -> Value {
    let parents = match &s.parents {
        Some((a, b)) => json!([a, b]),
        None => Value::Null,
    };
    json!({
        "id": s.id,
        "resolution": s.edge,
        "backing": s.backing,
        "vitality": s.vitality,
        "coverage": s.coverage,
        "creator": s.creator,
        "parents": parents,
        "birth_ms": s.birth_ms,
        "genome": s.genome,
        "video": format!("/api/video/{}", s.id),
    })
}

// ---- GET /api/sheep/:id -----------------------------------------------------

/// Per-sheep detail: the flock entry plus per-frame coverage (from the
/// accumulator's tile counts) and hall status. Read-only projection.
async fn get_sheep(State(st): State<HttpState>, AxPath(id): AxPath<String>) -> Response {
    let (mut obj, edge) = {
        let read = st.read.lock().unwrap();
        match read.live.get(&id) {
            Some(s) => (sheep_json(s), s.edge),
            None => {
                // Not in the live flock — maybe enshrined (dead but in the Hall).
                if let Some(h) = read.hall.iter().find(|h| h.id == id) {
                    let body = json!({
                        "id": h.id,
                        "resolution": h.edge,
                        "alive": false,
                        "hall": true,
                        "birth_ms": h.birth_ms,
                        "death_ms": h.death_ms,
                        "lifespan_ms": h.lifespan_ms,
                        "peak_backing": h.peak_backing,
                        "video": format!("/api/video/{}", h.id),
                    });
                    return ([snapshot_cache()], Json(body)).into_response();
                }
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "sheep not found" })),
                )
                    .into_response();
            }
        }
    };

    // Per-frame coverage (tile density) + total sample count from the
    // accumulator's merged frames. `total_count` is the sum of every folded
    // tile's sample count (density), held in always-resident metadata (no buffer
    // load), so this stays a cheap read cache.
    let (frames_coverage, samples): (Vec<usize>, u64) = {
        let accum = st.accum.lock().unwrap();
        let fc = (0..st.n_frames).map(|f| accum.tile_count(&id, f)).collect();
        (fc, accum.total_count(&id))
    };
    let total_tiles: usize = frames_coverage.iter().sum();
    // §1.1 density measure: mean samples per output pixel across the whole loop
    // (`edge*edge` pixels per frame × `N_FRAMES` frames). Higher = the "solid
    // object" flam3 parity the render-quality work targets. Guarded against a
    // zero-edge (degenerate) sheep so we never divide by zero.
    let total_pixels = (edge as u64) * (edge as u64) * (crate::spec::N_FRAMES as u64);
    let samples_per_pixel = if edge > 0 && total_pixels > 0 {
        samples as f64 / total_pixels as f64
    } else {
        0.0
    };

    if let Some(o) = obj.as_object_mut() {
        o.insert("alive".into(), json!(true));
        o.insert("hall".into(), json!(false));
        o.insert("frames_coverage".into(), json!(frames_coverage));
        o.insert("accumulated_tiles".into(), json!(total_tiles));
        o.insert("samples".into(), json!(samples));
        o.insert("samples_per_pixel".into(), json!(samples_per_pixel));
        o.insert("frame_edge".into(), json!(edge));
    }
    ([snapshot_cache()], Json(obj)).into_response()
}

// ---- GET /api/video/:id -----------------------------------------------------

/// Tonemap the accumulator's merged frames → encode a short loop video (via the
/// coordinator's ffmpeg path, ported to read the in-memory accumulator) → cache
/// it on disk → serve with Cache-Control. Regenerated when coverage advances
/// materially (a tile-count step). Returns 404 until any frame has density (or
/// if ffmpeg is unavailable — a missing encoder degrades to 404, never a crash).
async fn get_video(State(st): State<HttpState>, AxPath(id): AxPath<String>) -> Response {
    // Encode on a blocking thread: tonemap + ffmpeg are CPU/IO heavy.
    let accum = st.accum.clone();
    let data_dir = st.data_dir.clone();
    let n_frames = st.n_frames;
    let id_c = id.clone();
    let res = tokio::task::spawn_blocking(move || {
        video::ensure_video(&accum, &data_dir, &id_c, n_frames)
    })
    .await;

    match res {
        Ok(Ok(path)) => match std::fs::read(&path) {
            Ok(bytes) => (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, "video/webm".to_string()),
                    (
                        header::CACHE_CONTROL,
                        format!("public, max-age={VIDEO_MAX_AGE}"),
                    ),
                ],
                bytes,
            )
                .into_response(),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("read video: {e}") })),
            )
                .into_response(),
        },
        // No frames yet, or ffmpeg unavailable → 404 (graceful, §5/coordinator).
        Ok(Err(e)) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": e })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("encode task: {e}") })),
        )
            .into_response(),
    }
}

// ---- GET /api/hall ----------------------------------------------------------

/// The Hall of Fame (`engine.hall()`): enshrined dead sheep, preserved after
/// death (§2.2). Read-only projection.
async fn get_hall(State(st): State<HttpState>) -> Response {
    let read = st.read.lock().unwrap();
    let sheep: Vec<Value> = read
        .hall
        .iter()
        .map(|h| {
            json!({
                "id": h.id,
                "resolution": h.edge,
                "birth_ms": h.birth_ms,
                "death_ms": h.death_ms,
                "lifespan_ms": h.lifespan_ms,
                "peak_backing": h.peak_backing,
                "video": format!("/api/video/{}", h.id),
            })
        })
        .collect();
    ([snapshot_cache()], Json(json!({ "sheep": sheep }))).into_response()
}

// ---- POST /api/msg (the §10 1:1 write skin) ---------------------------------

/// §10 **writes (contribute)** — the mechanically-1:1 REST skin over the
/// protocol messages. The body is a signed [`Envelope`] (or a JSON array of
/// them); each is `deserialize JSON → verify sig → run the SAME handler the
/// libp2p path runs (selected by `env.t`) → serialize the response`. There is
/// NO per-type semantics here: routing is entirely by `env.t` through the run
/// loop's [`Control::Inject`], which calls `engine.apply` and re-publishes to
/// gossip exactly as an inbound gossip message would (and feeds `PieceUpload`
/// to the accumulator). The browser signs the same bytes a native peer signs,
/// so this is a transport adapter, never a second protocol (§10).
///
/// Signature is verified HERE (reject bad sig → 400) before the envelope is
/// handed to the loop. The §6.1 gateway ingest-audit (verify-before-vouch for
/// disposable browser identities) is applied inside the loop, per `ServeConfig`.
async fn post_msg(State(st): State<HttpState>, body: axum::body::Bytes) -> Response {
    let Some(cmd) = st.cmd.clone() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "write face disabled on this node" })),
        )
            .into_response();
    };

    // Accept a single Envelope or an array of them.
    let envs: Vec<Envelope> = match serde_json::from_slice::<Envelope>(&body) {
        Ok(e) => vec![e],
        Err(_) => match serde_json::from_slice::<Vec<Envelope>>(&body) {
            Ok(v) => v,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("not an Envelope or array: {e}") })),
                )
                    .into_response();
            }
        },
    };
    if envs.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "empty submission" })),
        )
            .into_response();
    }

    // Verify EVERY signature up front — a bad sig in the batch is a 400 (§10:
    // "verify its signature; reject bad sig"). This mirrors `engine.apply`'s
    // own `env.verify()` gate so the skin can't inject unsigned/forged bytes.
    for env in &envs {
        if !env.verify() {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "bad signature", "from": env.from })),
            )
                .into_response();
        }
    }

    // Route each through the loop's one code path (apply + vouch/gossip).
    let mut results = Vec::with_capacity(envs.len());
    for env in envs {
        let (tx, rx) = oneshot::channel();
        if cmd.send(Control::Inject(env, tx)).is_err() {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "run loop unavailable" })),
            )
                .into_response();
        }
        match rx.await {
            Ok(r) => results.push(r),
            Err(_) => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({ "error": "run loop dropped the request" })),
                )
                    .into_response();
            }
        }
    }

    let all_accepted = results.iter().all(|r| r.accepted);
    let items: Vec<Value> = results.iter().map(inject_json).collect();
    let status = if all_accepted {
        StatusCode::OK
    } else {
        // At least one rejected (audit mismatch / apply-rejected). 202-ish
        // semantics would be ambiguous; report 200 with per-item accepted flags
        // when ANY accepted, else 422 when ALL rejected (clear client signal).
        if results.iter().any(|r| r.accepted) {
            StatusCode::OK
        } else {
            StatusCode::UNPROCESSABLE_ENTITY
        }
    };
    // Single-envelope submissions get a flat object; arrays get `results`.
    let body = if items.len() == 1 {
        items.into_iter().next().unwrap()
    } else {
        json!({ "results": items })
    };
    (status, Json(body)).into_response()
}

/// JSON projection of one [`InjectResult`] — accepted/rejected + resulting
/// standing (credits/coverage/backing) read back from the shared engine state.
fn inject_json(r: &InjectResult) -> Value {
    let mut o = json!({
        "accepted": r.accepted,
        "credits": r.credits,
        // The submitting key's running confirmed-tile total + the constant
        // tile→credit rate (§3) — so a browser contributor can show "Accepted
        // tiles" and its progress to the next credit
        // (`confirmed_tiles % tiles_per_credit` toward the next, `confirmed_tiles
        // / tiles_per_credit` earned). `tiles_per_credit` is the spec.rs constant
        // (`TILES_PER_CREDIT`), emitted directly (it never varies per result).
        "confirmed_tiles": r.confirmed_tiles,
        "tiles_per_credit": crate::spec::TILES_PER_CREDIT,
    });
    if let Some(reason) = &r.reason {
        o["reason"] = json!(reason);
    }
    if let Some(sheep) = &r.sheep_id {
        o["sheep_id"] = json!(sheep);
        o["coverage"] = json!(r.coverage);
        o["backing"] = json!(r.backing);
    }
    o
}

// ---- GET /api/assign (advisory work hand-out, §10) --------------------------

#[derive(serde::Deserialize)]
struct AssignParams {
    /// Worker public key, lowercase hex — whose advisory work to compute.
    #[serde(rename = "pub")]
    pubkey: String,
    /// How many blocks to hand out (advisory; defaults to a small bundle).
    #[serde(default)]
    want: Option<u32>,
}

/// §10 **advisory work hand-out** — so a browser worker knows what to render.
/// Computes, for `?pub=<hex>`, the engine's least-covered uncapped blocks
/// (`AssignResp.blocks`) + the audit tiles that pubkey is assigned (§6,
/// `AssignResp.audits`) — reusing the engine's block-selection + audit-
/// assignment logic via [`Control::Assign`]. Read-only / advisory (§10): a race
/// just yields a duplicate render that determinism dedups, so it is safe.
async fn get_assign(State(st): State<HttpState>, Query(p): Query<AssignParams>) -> Response {
    let Some(cmd) = st.cmd.clone() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "assign face disabled on this node" })),
        )
            .into_response();
    };
    if p.pubkey.len() != 64 || !p.pubkey.bytes().all(|b| b.is_ascii_hexdigit()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "pub must be 64-char hex" })),
        )
            .into_response();
    }
    let want = p.want.unwrap_or(4).min(64);
    let (tx, rx) = oneshot::channel();
    if cmd.send(Control::Assign(p.pubkey, want, tx)).is_err() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "run loop unavailable" })),
        )
            .into_response();
    }
    let Ok(res) = rx.await else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "run loop dropped the request" })),
        )
            .into_response();
    };
    ([snapshot_cache()], Json(assign_json(&res))).into_response()
}

/// JSON in the §10 `AssignResp` shape: advisory blocks (each a `(sheep, frame,
/// idx, pass)` unit with its block id) + assigned audit tiles.
fn assign_json(res: &AssignResult) -> Value {
    let blocks: Vec<Value> = res
        .blocks
        .iter()
        .map(|(block_id, sheep, frame, idx, pass)| {
            json!({
                "block_id": block_id,
                "sheep_id": sheep,
                "frame": frame,
                "idx": idx,
                "pass": pass,
            })
        })
        .collect();
    let audits: Vec<Value> = res
        .audits
        .iter()
        .map(|(sheep, frame, idx, pass)| {
            json!({
                "sheep_id": sheep,
                "frame": frame,
                "idx": idx,
                "pass": pass,
            })
        })
        .collect();
    json!({ "blocks": blocks, "audits": audits })
}

/// Health probe: ok + live-flock size, so a monitor can confirm the watch face
/// is serving.
async fn get_health(State(st): State<HttpState>) -> Json<Value> {
    let read = st.read.lock().unwrap();
    Json(json!({
        "status": "ok",
        "self": read.self_pub,
        "live_flock": read.live.len(),
        "hall": read.hall.len(),
    }))
}
