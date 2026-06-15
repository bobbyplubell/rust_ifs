//! proof-of-sheep v2 coordinator.
//!
//! axum + tokio REST service (no WebSocket, no SSE). Runs the GA, hands out
//! deterministic render work-units, verifies + merges contributions natively
//! via flame-core, and serves merged loop videos. See API.md for the contract.

mod audit;
mod auth;
mod db;
mod disk;
mod error;
mod ga;
mod ga_config;
mod histio;
mod render;
mod routes;
mod spec;
mod state;
mod video;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
use axum::Router;
use tower_http::cors::{Any, CorsLayer};

use crate::db::Db;
use crate::ga::now_ms;
use crate::state::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "coordinator=info,tower_http=info".into()),
        )
        .init();

    let bind = std::env::var("BIND").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let data_dir = PathBuf::from(std::env::var("DATA_DIR").unwrap_or_else(|_| "./data".into()));
    let genomes_dir = std::env::var("GENOMES_DIR").unwrap_or_else(|_| "../web/genomes".into());
    let gen_ms: u64 = std::env::var("GEN_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(spec::GEN_MS_DEFAULT);

    std::fs::create_dir_all(&data_dir)?;
    let db_path = data_dir.join("coordinator.sqlite");
    let db = Db::open(db_path.to_str().unwrap())?;

    // Initialize the meta singleton + seed the flock once.
    {
        let conn = db.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO meta (id, gen, gen_started_ms, gen_ms) VALUES (0, 0, ?1, ?2)",
            rusqlite::params![now_ms() as i64, gen_ms as i64],
        )?;
        // Keep gen_ms in sync with env on restart.
        conn.execute("UPDATE meta SET gen_ms = ?1 WHERE id = 0", [gen_ms as i64])?;
    }
    // The GA "personality" for this world (mutation/immigrants/selection),
    // logged at boot so the active config is visible per world.
    let ga_config = ga_config::GaConfig::from_env();

    ga::seed_flock(&db, &genomes_dir, &ga_config)
        .map_err(|e| anyhow::anyhow!("seed flock failed: {}", e.msg))?;

    let disk = disk::DiskConfig::from_env();
    let state = Arc::new(AppState { db, data_dir, disk, ga: ga_config });

    // Background GA tick: when the generation clock expires, run a tick.
    spawn_ga_loop(state.clone());

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .route("/api/flock", get(routes::get_flock))
        .route("/api/sheep/:id", get(routes::get_sheep))
        .route("/api/video/:id", get(routes::get_video))
        .route("/api/assign", post(routes::post_assign))
        .route("/api/submit", post(routes::post_submit))
        .route("/api/vote", post(routes::post_vote))
        .route("/api/breed", post(routes::post_breed))
        .route("/api/me", get(routes::get_me))
        .route("/api/hall", get(routes::get_hall))
        .route("/api/stats", get(routes::get_stats))
        .route("/health", get(routes::get_health))
        // A submit can carry up to 64 full-frame histograms (base64 of
        // compressed u64 cells). Raise axum's 2MB default; per-hist decode is
        // still bounded to the spec size in `histio`, and results are capped at
        // 64 in the handler, so this is a coarse outer guard, not the real cap.
        .layer(DefaultBodyLimit::max(96 * 1024 * 1024))
        .layer(cors)
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!("coordinator listening on {bind}");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Poll the generation clock once a minute; run a GA tick when it expires.
fn spawn_ga_loop(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(60));
        loop {
            ticker.tick().await;
            let expired = {
                let conn = state.db.conn.lock().unwrap();
                let row: Result<(i64, i64), _> = conn.query_row(
                    "SELECT gen_started_ms, gen_ms FROM meta WHERE id = 0",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                );
                match row {
                    Ok((started, gen_ms)) => now_ms() as i64 - started >= gen_ms,
                    Err(_) => false,
                }
            };
            if expired {
                match ga::tick(&state.db, &state.ga) {
                    Ok(g) => tracing::info!("GA tick: advanced to generation {g}"),
                    Err(e) => tracing::error!("GA tick failed: {}", e.msg),
                }
            }
        }
    });
}
