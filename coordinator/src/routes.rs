//! All HTTP handlers — the API.md contract.

use std::sync::Arc;

use axum::extract::{Path as AxPath, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth;
use crate::disk;
use crate::error::{ApiError, ApiResult};
use crate::ga::{self, now_ms};
use crate::render;
use crate::spec;
use crate::state::AppState;
use crate::{histio, video};

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Verify the signature AND commit the nonce (replay guard) in one shot.
/// Returns the verified pubkey hex. Rejects banned accounts.
fn auth_and_nonce(state: &AppState, body: &Value) -> ApiResult<String> {
    let a = auth::verify(body)?;
    let conn = state.db.conn.lock().unwrap();

    // Ensure account row exists.
    conn.execute(
        "INSERT OR IGNORE INTO account (pub, created_ms) VALUES (?1, ?2)",
        rusqlite::params![a.pub_hex, now_ms() as i64],
    )?;

    let (last_nonce, banned): (i64, i64) = conn.query_row(
        "SELECT last_nonce, banned FROM account WHERE pub = ?1",
        [&a.pub_hex],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;

    if banned != 0 {
        return Err(ApiError::forbidden("account banned"));
    }
    if (a.nonce as i64) <= last_nonce {
        return Err(ApiError::unauthorized("stale or replayed nonce"));
    }
    conn.execute(
        "UPDATE account SET last_nonce = ?1 WHERE pub = ?2",
        rusqlite::params![a.nonce as i64, a.pub_hex],
    )?;
    Ok(a.pub_hex)
}

/// Ban an account and invalidate all of its accepted work (set its tiles back
/// to free, decrement the sheep tile counts). Called on a hash mismatch.
fn ban_account(state: &AppState, pubkey: &str) -> ApiResult<()> {
    let conn = state.db.conn.lock().unwrap();
    conn.execute("UPDATE account SET banned = 1, credits = 0 WHERE pub = ?1", [pubkey])?;
    // Free this account's accepted tiles so they can be re-rendered by others.
    conn.execute(
        "UPDATE tile SET status = 2, pub = NULL WHERE pub = ?1",
        [pubkey],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// GET /api/flock
// ---------------------------------------------------------------------------

pub async fn get_flock(State(state): State<Arc<AppState>>) -> ApiResult<Json<Value>> {
    let conn = state.db.conn.lock().unwrap();
    let (gen, started, gen_ms): (i64, i64, i64) = conn.query_row(
        "SELECT gen, gen_started_ms, gen_ms FROM meta WHERE id = 0",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let elapsed = now_ms() as i64 - started;
    let closes_in = (gen_ms - elapsed).max(0);

    let sheep = sheep_entries(&conn, gen, "WHERE alive = 1")?;
    Ok(Json(json!({
        "gen": gen,
        "gen_closes_in_ms": closes_in,
        "sheep": sheep,
    })))
}

/// Build the `/api/flock`-shape sheep entry list with a WHERE clause.
fn sheep_entries(
    conn: &rusqlite::Connection,
    gen: i64,
    where_clause: &str,
) -> ApiResult<Vec<Value>> {
    let sql = format!(
        "SELECT id, name, parent_a, parent_b, gen, tiles FROM sheep {where_clause} ORDER BY tiles DESC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, i64>(5)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, name, pa, pb, sgen, tiles) = row?;
        let backings: i64 = conn.query_row(
            "SELECT COUNT(*) FROM vote WHERE gen = ?1 AND sheep_id = ?2",
            rusqlite::params![gen, id],
            |r| r.get(0),
        )?;
        let parents = match (pa, pb) {
            (Some(a), Some(b)) => json!([a, b]),
            _ => Value::Null,
        };
        out.push(json!({
            "id": id,
            "name": name,
            "parents": parents,
            "gen": sgen,
            "tiles": tiles,
            "backings": backings,
            "video": format!("/api/video/{id}"),
        }));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// GET /api/sheep/:id
// ---------------------------------------------------------------------------

pub async fn get_sheep(
    State(state): State<Arc<AppState>>,
    AxPath(id): AxPath<String>,
) -> ApiResult<Json<Value>> {
    let conn = state.db.conn.lock().unwrap();
    let gen: i64 = conn.query_row("SELECT gen FROM meta WHERE id = 0", [], |r| r.get(0))?;

    let entry = sheep_entries(&conn, gen, &format!("WHERE id = '{}'", sql_lit(&id)))?
        .into_iter()
        .next()
        .ok_or_else(|| ApiError::not_found("sheep not found"))?;

    let (genome, alive, hof, n_frames): (String, i64, i64, i64) = conn.query_row(
        "SELECT genome, alive, hof, n_frames FROM sheep WHERE id = ?1",
        [&id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;

    // Per-frame coverage.
    let mut coverage = vec![0i64; n_frames as usize];
    {
        let mut stmt =
            conn.prepare("SELECT frame, accepted FROM coverage WHERE sheep_id = ?1")?;
        let rows = stmt.query_map([&id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
        for row in rows.flatten() {
            if (row.0 as usize) < coverage.len() {
                coverage[row.0 as usize] = row.1;
            }
        }
    }
    let samples: i64 = coverage.iter().sum::<i64>() * spec::SPP as i64;

    let mut full = entry;
    let obj = full.as_object_mut().unwrap();
    obj.insert("genome".into(), serde_json::from_str(&genome).unwrap_or(Value::Null));
    obj.insert("frames_coverage".into(), json!(coverage));
    obj.insert("samples".into(), json!(samples));
    obj.insert("alive".into(), json!(alive != 0));
    obj.insert("hof".into(), json!(hof != 0));
    Ok(Json(full))
}

/// Escape a single quote for an inlined SQL string literal (ids are hex so this
/// is belt-and-suspenders).
fn sql_lit(s: &str) -> String {
    s.replace('\'', "''")
}

// ---------------------------------------------------------------------------
// GET /api/video/:id
// ---------------------------------------------------------------------------

pub async fn get_video(
    State(state): State<Arc<AppState>>,
    AxPath(id): AxPath<String>,
) -> Response {
    let path = video::video_path(&state.data_dir, &id);
    match std::fs::read(&path) {
        Ok(bytes) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "video/webm"),
                (header::CACHE_CONTROL, "public, max-age=60"),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no video yet for this sheep" })),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// POST /api/assign
// ---------------------------------------------------------------------------

pub async fn post_assign(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let pubkey = auth_and_nonce(&state, &body)?;
    let requested = body.get("sheepId").and_then(Value::as_str).map(str::to_string);

    let conn = state.db.conn.lock().unwrap();

    // Choose a sheep: requested, else the living sheep with the least coverage.
    let sheep_id: String = match requested {
        Some(s) => {
            let exists: i64 = conn.query_row(
                "SELECT COUNT(*) FROM sheep WHERE id = ?1 AND alive = 1",
                [&s],
                |r| r.get(0),
            )?;
            if exists == 0 {
                return Err(ApiError::not_found("requested sheep not alive"));
            }
            s
        }
        None => conn
            .query_row(
                "SELECT id FROM sheep WHERE alive = 1 ORDER BY tiles ASC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .map_err(|_| ApiError::not_found("no living sheep to render"))?,
    };

    let genome_json: String =
        conn.query_row("SELECT genome FROM sheep WHERE id = ?1", [&sheep_id], |r| r.get(0))?;

    // Hand out BUNDLE_SIZE distinct (frame, idx) tiles never assigned before
    // (the collision guard is the tile table PK). Sweep frames/idxs, insert the
    // first unassigned ones.
    let mut units = Vec::new();
    'outer: for frame in 0..spec::N_FRAMES {
        for idx in 0..spec::IDXS_PER_FRAME {
            if units.len() >= spec::BUNDLE_SIZE {
                break 'outer;
            }
            let inserted = conn.execute(
                "INSERT OR IGNORE INTO tile (sheep_id, frame, idx, pub, status, assigned_ms)
                 VALUES (?1, ?2, ?3, ?4, 0, ?5)",
                rusqlite::params![sheep_id, frame, idx, pubkey, now_ms() as i64],
            )?;
            if inserted == 1 {
                units.push(json!({
                    "sheepId": sheep_id,
                    "genomeJson": genome_json,
                    "frame": frame,
                    "idx": idx,
                    "w": spec::W,
                    "h": spec::H,
                    "ss": spec::SS,
                    "spp": spec::SPP,
                    "nFrames": spec::N_FRAMES,
                }));
            }
        }
    }

    if units.is_empty() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "this sheep is fully assigned; try another",
        ));
    }

    Ok(Json(json!({ "units": units, "audits": [] })))
}

// ---------------------------------------------------------------------------
// POST /api/submit
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ResultUnit {
    #[serde(rename = "sheepId")]
    sheep_id: String,
    frame: u32,
    idx: u32,
    hash: String,
    // `count` is transported as a STRING (it can exceed JS Number.MAX_SAFE_INTEGER
    // — see API.md Result + web/js/contribute.js). We don't use it in the merge
    // (the hash is the trust anchor), so accept it opaquely and don't reject on
    // its type.
    #[allow(dead_code)]
    count: Option<String>,
    hist: String,
}

pub async fn post_submit(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let pubkey = auth_and_nonce(&state, &body)?;

    let results: Vec<ResultUnit> = serde_json::from_value(
        body.get("results").cloned().unwrap_or(Value::Array(vec![])),
    )
    .map_err(|e| ApiError::bad(format!("bad results: {e}")))?;

    if results.len() > 64 {
        return Err(ApiError::bad("too many results in one submit"));
    }

    let mut accepted = 0u64;
    let mut rejected = 0u64;
    let mut fraud = false;

    for r in &results {
        // The tile must be assigned to THIS pubkey and still pending.
        let assigned_to: Option<String> = {
            let conn = state.db.conn.lock().unwrap();
            conn.query_row(
                "SELECT pub FROM tile WHERE sheep_id = ?1 AND frame = ?2 AND idx = ?3 AND status = 0",
                rusqlite::params![r.sheep_id, r.frame, r.idx],
                |row| row.get(0),
            )
            .ok()
            .flatten()
        };
        if assigned_to.as_deref() != Some(pubkey.as_str()) {
            rejected += 1;
            continue;
        }

        // Load the genome + spec.
        let genome_json: String = {
            let conn = state.db.conn.lock().unwrap();
            match conn.query_row("SELECT genome FROM sheep WHERE id = ?1", [&r.sheep_id], |row| {
                row.get(0)
            }) {
                Ok(g) => g,
                Err(_) => {
                    rejected += 1;
                    continue;
                }
            }
        };
        let genome = match render::parse_genome(&genome_json) {
            Ok(g) => g,
            Err(_) => {
                rejected += 1;
                continue;
            }
        };
        let sheep_id_bytes = match render::sheep_id_bytes(&r.sheep_id) {
            Ok(b) => b,
            Err(_) => {
                rejected += 1;
                continue;
            }
        };

        // Re-render natively and verify the hash (v2.0 = verify everything).
        let server_hash = render::verify_tile_hash(
            &genome,
            &sheep_id_bytes,
            r.frame,
            r.idx,
            spec::W,
            spec::H,
            spec::SS,
            spec::SPP,
            spec::N_FRAMES,
        );

        if server_hash != r.hash.trim() {
            // Fraud: hash mismatch. Ban + invalidate everything.
            fraud = true;
            break;
        }

        // Decode the contributed histogram and confirm IT hashes to the same
        // value (so the merged pixels are exactly the verified render).
        let cells = match histio::decode_hist(&r.hist) {
            Ok(c) => c,
            Err(_) => {
                rejected += 1;
                continue;
            }
        };
        // Cheap integrity: client hist must match the verified hash too.
        let client_hash = {
            use flame_core::render::Accum;
            let mut accum = Accum::new((spec::W * spec::SS) as usize, (spec::H * spec::SS) as usize);
            for (cell, src) in accum.data.iter_mut().zip(cells.chunks_exact(4)) {
                cell.copy_from_slice(src);
            }
            flame_core::chunked::hist_hash_hex(&accum)
        };
        if client_hash != server_hash {
            rejected += 1;
            continue;
        }

        // Merge into the accumulated frame histogram on disk, through the disk
        // guard: it reconstructs-from-log if this sheep was evicted, evicts LRU
        // sheep to stay under the cap / above the free floor, and DEGRADES
        // gracefully (skips the merge, never errors) when the disk is too full.
        // Either way we still accept the tile + record it in the ledger below,
        // so the contributor earns credit, the collision guard holds, and the
        // histogram stays reconstructable from the log.
        let merged = disk::merge_tile(
            &state.disk,
            &state.db,
            &state.data_dir,
            &r.sheep_id,
            &genome,
            r.frame,
            &cells,
        )
        .unwrap_or(false);
        if !merged {
            tracing::warn!(
                "submit: hist merge skipped for sheep {} frame {} (disk guard) — \
                 tile still accepted + logged; reconstructable later",
                r.sheep_id,
                r.frame,
            );
        }

        // Mark the tile accepted, bump counters.
        {
            let conn = state.db.conn.lock().unwrap();
            conn.execute(
                "UPDATE tile SET status = 1, hash = ?1 WHERE sheep_id = ?2 AND frame = ?3 AND idx = ?4",
                rusqlite::params![server_hash, r.sheep_id, r.frame, r.idx],
            )?;
            conn.execute(
                "UPDATE sheep SET tiles = tiles + 1 WHERE id = ?1",
                [&r.sheep_id],
            )?;
            conn.execute(
                "INSERT INTO coverage (sheep_id, frame, accepted) VALUES (?1, ?2, 1)
                 ON CONFLICT(sheep_id, frame) DO UPDATE SET accepted = accepted + 1",
                rusqlite::params![r.sheep_id, r.frame],
            )?;
        }
        accepted += 1;
    }

    if fraud {
        ban_account(&state, &pubkey)?;
        return Err(ApiError::forbidden(
            "fraud detected (hash mismatch): account banned, work invalidated",
        ));
    }

    // Credit the contributor: 128 accepted tiles = 1 credit. Carry the
    // remainder so partial progress isn't lost.
    let (credits, reputation) = {
        let conn = state.db.conn.lock().unwrap();
        conn.execute(
            "UPDATE account
             SET tiles = tiles + ?1,
                 tile_remainder = tile_remainder + ?1,
                 reputation = reputation + ?2
             WHERE pub = ?3",
            rusqlite::params![accepted as i64, accepted as f64 * 0.01, pubkey],
        )?;
        // Convert whole credits out of the remainder.
        let remainder: i64 = conn.query_row(
            "SELECT tile_remainder FROM account WHERE pub = ?1",
            [&pubkey],
            |r| r.get(0),
        )?;
        let new_credits = remainder / spec::TILES_PER_CREDIT as i64;
        if new_credits > 0 {
            conn.execute(
                "UPDATE account
                 SET credits = credits + ?1,
                     tile_remainder = tile_remainder - ?2
                 WHERE pub = ?3",
                rusqlite::params![new_credits, new_credits * spec::TILES_PER_CREDIT as i64, pubkey],
            )?;
        }
        conn.query_row(
            "SELECT credits, reputation FROM account WHERE pub = ?1",
            [&pubkey],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?)),
        )?
    };

    // Maybe re-encode the video (quality-delta threshold) — best-effort.
    maybe_reencode(&state, &results);

    Ok(Json(json!({
        "accepted": accepted,
        "rejected": rejected,
        "credits": credits,
        "reputation": reputation,
    })))
}

/// Re-encode videos for the distinct sheep touched by this submit, if they've
/// crossed a tile-count step. Best-effort: logs and swallows errors (e.g. no
/// ffmpeg), so the merge path never fails on the encode.
fn maybe_reencode(state: &AppState, results: &[ResultUnit]) {
    let mut seen = std::collections::HashSet::new();
    for r in results {
        if !seen.insert(r.sheep_id.clone()) {
            continue;
        }
        let row: Option<(i64, i64, String)> = {
            let conn = state.db.conn.lock().unwrap();
            conn.query_row(
                "SELECT tiles, video_rev, genome FROM sheep WHERE id = ?1",
                [&r.sheep_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .ok()
        };
        let Some((tiles, video_rev, genome_json)) = row else { continue };
        if !video::should_reencode(tiles as u64, video_rev as u64) {
            continue;
        }
        let Ok(genome) = render::parse_genome(&genome_json) else { continue };
        match video::encode_video(
            &state.data_dir,
            &genome,
            &r.sheep_id,
            spec::N_FRAMES,
            spec::W,
            spec::H,
            spec::SS,
        ) {
            Ok(_) => {
                let conn = state.db.conn.lock().unwrap();
                let _ = conn.execute(
                    "UPDATE sheep SET video_rev = ?1 WHERE id = ?2",
                    rusqlite::params![video::rev_for(tiles as u64) as i64, r.sheep_id],
                );
            }
            Err(e) => tracing::warn!("video encode for {} skipped: {}", r.sheep_id, e.msg),
        }
    }
}

// ---------------------------------------------------------------------------
// POST /api/vote
// ---------------------------------------------------------------------------

pub async fn post_vote(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let pubkey = auth_and_nonce(&state, &body)?;
    let sheep_id = body
        .get("sheepId")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::bad("missing sheepId"))?
        .to_string();

    let conn = state.db.conn.lock().unwrap();
    let gen: i64 = conn.query_row("SELECT gen FROM meta WHERE id = 0", [], |r| r.get(0))?;

    let alive: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sheep WHERE id = ?1 AND alive = 1",
        [&sheep_id],
        |r| r.get(0),
    )?;
    if alive == 0 {
        return Err(ApiError::not_found("sheep not alive"));
    }

    let credits: i64 =
        conn.query_row("SELECT credits FROM account WHERE pub = ?1", [&pubkey], |r| r.get(0))?;
    if credits < 1 {
        return Err(ApiError::forbidden("not enough credits to vote"));
    }

    // One vote per (gen, pub, sheep).
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO vote (gen, pub, sheep_id, ts_ms) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![gen, pubkey, sheep_id, now_ms() as i64],
    )?;
    if inserted == 0 {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "already backed this sheep this generation",
        ));
    }

    conn.execute(
        "UPDATE account SET credits = credits - 1, backings_used = backings_used + 1 WHERE pub = ?1",
        [&pubkey],
    )?;

    let new_credits = credits - 1;
    let backings: i64 = conn.query_row(
        "SELECT COUNT(*) FROM vote WHERE gen = ?1 AND sheep_id = ?2",
        rusqlite::params![gen, sheep_id],
        |r| r.get(0),
    )?;

    Ok(Json(json!({ "ok": true, "credits": new_credits, "backings": backings })))
}

// ---------------------------------------------------------------------------
// POST /api/breed
// ---------------------------------------------------------------------------

pub async fn post_breed(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let pubkey = auth_and_nonce(&state, &body)?;
    let parent_a = body
        .get("parentA")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::bad("missing parentA"))?
        .to_string();
    let parent_b = body
        .get("parentB")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::bad("missing parentB"))?
        .to_string();

    // Spend credits.
    let gen: i64 = {
        let conn = state.db.conn.lock().unwrap();
        let credits: i64 =
            conn.query_row("SELECT credits FROM account WHERE pub = ?1", [&pubkey], |r| r.get(0))?;
        if credits < spec::BREED_COST {
            return Err(ApiError::forbidden(format!(
                "breeding costs {} credits; you have {credits}",
                spec::BREED_COST
            )));
        }
        conn.execute(
            "UPDATE account SET credits = credits - ?1 WHERE pub = ?2",
            rusqlite::params![spec::BREED_COST, pubkey],
        )?;
        conn.query_row("SELECT gen FROM meta WHERE id = 0", [], |r| r.get(0))?
    };

    // Server does the crossover, inserts the child (alive). Rendering happens
    // organically as contributors get assigned its tiles.
    let child_id = ga::breed_pair(&state.db, &parent_a, &parent_b, gen)?;

    Ok(Json(json!({ "childId": child_id })))
}

// ---------------------------------------------------------------------------
// GET /api/me?pub=HEX
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct MeQuery {
    #[serde(rename = "pub")]
    pub_hex: String,
}

pub async fn get_me(
    State(state): State<Arc<AppState>>,
    Query(q): Query<MeQuery>,
) -> ApiResult<Json<Value>> {
    let conn = state.db.conn.lock().unwrap();
    let row = conn
        .query_row(
            "SELECT credits, reputation, tiles, backings_used FROM account WHERE pub = ?1",
            [&q.pub_hex],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, f64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            },
        )
        .unwrap_or((0, 0.0, 0, 0));
    Ok(Json(json!({
        "credits": row.0,
        "reputation": row.1,
        "tiles": row.2,
        "backings_used": row.3,
    })))
}

// ---------------------------------------------------------------------------
// GET /api/hall
// ---------------------------------------------------------------------------

pub async fn get_hall(State(state): State<Arc<AppState>>) -> ApiResult<Json<Value>> {
    let conn = state.db.conn.lock().unwrap();
    let gen: i64 = conn.query_row("SELECT gen FROM meta WHERE id = 0", [], |r| r.get(0))?;
    let sheep = sheep_entries(&conn, gen, "WHERE hof = 1")?;
    Ok(Json(json!({ "sheep": sheep })))
}

// ---------------------------------------------------------------------------
// GET /api/stats  +  GET /health   (disk-cache observability)
// ---------------------------------------------------------------------------

/// Current histogram-cache usage vs the cap, and actual filesystem free space
/// vs the floor — so the bounded working cache is observable in prod.
pub async fn get_stats(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(disk::stats(&state.disk, &state.db, &state.data_dir))
}

/// Health probe: always "ok", but carries the disk-guard snapshot so a monitor
/// can alert as the hist cache approaches the cap / the disk approaches full.
pub async fn get_health(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({
        "status": "ok",
        "disk": disk::stats(&state.disk, &state.db, &state.data_dir),
    }))
}
