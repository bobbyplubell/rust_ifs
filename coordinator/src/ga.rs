//! The server-side genetic algorithm.
//!
//! On the generation boundary: tally this gen's votes → keep the top survivors
//! → breed survivors (flame-core crossover+mutation) → inject fresh random
//! immigrants → cull the rest. Plus `breed_pair` for the `/breed` propose-a-
//! pairing endpoint. Genomes are ALWAYS authored here (never client-supplied),
//! which is the content-integrity guarantee.

use std::time::{SystemTime, UNIX_EPOCH};

use flame_core::canonical::{canonical_json, sheep_id_hex};
use flame_core::genome::Genome;
use flame_core::rng::Rng;

use crate::db::Db;
use crate::error::{ApiError, ApiResult};
use crate::ga_config::GaConfig;
use crate::spec;

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// Breed two parents into a child, honoring the world's GA personality
/// (mutation rate + magnitude). This reconstructs `flame_core::breed::breed`
/// (crossover → mutate → auto_frame) but with the mutation knobs sourced from
/// `cfg` instead of the library's hardcoded `BREED_MUTATION_RATE`. With the
/// default config (rate=0.15, magnitude=1) it is identical to `breed()`.
fn breed_with(ga: &Genome, gb: &Genome, seed: u64, cfg: &GaConfig) -> Genome {
    let mut rng = Rng::new(seed);
    let mut child = flame_core::breed::crossover(ga, gb, &mut rng);
    for _ in 0..cfg.mutation_magnitude.max(1) {
        flame_core::breed::mutate(&mut child, &mut rng, cfg.mutation_rate);
    }
    // Re-frame on the child's own attractor so bred sheep arrive centered.
    child.auto_frame();
    child
}

/// Insert a sheep row from a genome. Returns its sheep_id. Idempotent on id.
pub fn insert_sheep(
    db: &Db,
    genome: &Genome,
    name: &str,
    parents: Option<(&str, &str)>,
    gen: i64,
) -> ApiResult<String> {
    let json = canonical_json(genome);
    let id = sheep_id_hex(genome);
    let (pa, pb) = match parents {
        Some((a, b)) => (Some(a.to_string()), Some(b.to_string())),
        None => (None, None),
    };
    let conn = db.conn.lock().unwrap();
    conn.execute(
        "INSERT OR IGNORE INTO sheep
         (id, name, genome, parent_a, parent_b, gen, n_frames, w, h, ss, spp, alive, hof, tiles, created_ms, video_rev)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 1, 0, 0, ?12, 0)",
        rusqlite::params![
            id, name, json, pa, pb, gen,
            spec::N_FRAMES, spec::W, spec::H, spec::SS, spec::SPP, now_ms() as i64
        ],
    )?;
    Ok(id)
}

/// Seed the flock from `web/genomes/` if the sheep table is empty. Falls back to
/// generating random genomes if the directory isn't found.
pub fn seed_flock(db: &Db, genomes_dir: &str, cfg: &GaConfig) -> ApiResult<()> {
    {
        let conn = db.conn.lock().unwrap();
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM sheep", [], |r| r.get(0))?;
        if count > 0 {
            return Ok(());
        }
    }

    let mut seeded = 0;
    if let Ok(entries) = std::fs::read_dir(genomes_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            // Skip manifest / non-genome json.
            let fname = path.file_name().and_then(|f| f.to_str()).unwrap_or("");
            if fname == "manifest.json" {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(&path) {
                if let Ok(genome) = serde_json::from_str::<Genome>(&text) {
                    let name = format!("Sheep #{}", seeded + 1);
                    insert_sheep(db, &genome, &name, None, 0)?;
                    seeded += 1;
                }
            }
        }
    }

    // Top up with random immigrants if the genome dir was thin/missing.
    while seeded < cfg.flock_size as usize {
        let mut rng = Rng::new(now_ms().wrapping_add(seeded as u64 * 7919));
        let genome = Genome::random(&mut rng, 3);
        let name = format!("Sheep #{}", seeded + 1);
        insert_sheep(db, &genome, &name, None, 0)?;
        seeded += 1;
    }
    Ok(())
}

/// Breed two parents (by sheep_id) into a child genome, render-able afterward.
/// The breeding seed is derived from both ids so the same pairing is
/// deterministic. Returns the child's sheep_id (already inserted, alive).
pub fn breed_pair(
    db: &Db,
    parent_a: &str,
    parent_b: &str,
    gen: i64,
    cfg: &GaConfig,
) -> ApiResult<String> {
    let (ga, gb) = {
        let conn = db.conn.lock().unwrap();
        let load = |id: &str| -> ApiResult<Genome> {
            let json: String = conn
                .query_row("SELECT genome FROM sheep WHERE id = ?1", [id], |r| r.get(0))
                .map_err(|_| ApiError::not_found(format!("parent {id} not found")))?;
            serde_json::from_str(&json).map_err(|e| ApiError::internal(format!("bad genome: {e}")))
        };
        (load(parent_a)?, load(parent_b)?)
    };

    // Deterministic seed from the two ids.
    let mut seed_bytes = [0u8; 8];
    for (i, b) in parent_a.bytes().chain(parent_b.bytes()).enumerate() {
        seed_bytes[i % 8] ^= b;
    }
    let seed = u64::from_le_bytes(seed_bytes) ^ now_ms();

    let child = breed_with(&ga, &gb, seed, cfg);
    child
        .validate()
        .map_err(|e| ApiError::bad(format!("bred child invalid: {e}")))?;

    let name = child_name(db, gen)?;
    let id = insert_sheep(db, &child, &name, Some((parent_a, parent_b)), gen)?;
    Ok(id)
}

fn child_name(db: &Db, gen: i64) -> ApiResult<String> {
    let conn = db.conn.lock().unwrap();
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM sheep", [], |r| r.get(0))?;
    Ok(format!("Sheep #{} (g{gen})", n + 1))
}

/// Run one generation tick: tally votes, select survivors, breed, inject
/// immigrants, cull the losers, advance the gen counter. Returns the new gen.
pub fn tick(db: &Db, cfg: &GaConfig) -> ApiResult<i64> {
    let gen: i64 = {
        let conn = db.conn.lock().unwrap();
        conn.query_row("SELECT gen FROM meta WHERE id = 0", [], |r| r.get(0))?
    };

    // Rank living sheep by this gen's vote backings (ties broken by tiles).
    let ranked: Vec<(String, i64)> = {
        let conn = db.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT s.id,
                    (SELECT COUNT(*) FROM vote v WHERE v.gen = ?1 AND v.sheep_id = s.id) AS backings
             FROM sheep s
             WHERE s.alive = 1
             ORDER BY backings DESC, s.tiles DESC",
        )?;
        let rows = stmt.query_map([gen], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        rows.filter_map(Result::ok).collect()
    };

    let survivors: Vec<String> =
        ranked.iter().take(cfg.survivors as usize).map(|(id, _)| id.clone()).collect();

    // Enshrine the gen winner into the Hall of Fame (if it earned any votes).
    if let Some((winner, backings)) = ranked.first() {
        if *backings > 0 {
            let conn = db.conn.lock().unwrap();
            conn.execute("UPDATE sheep SET hof = 1 WHERE id = ?1", [winner])?;
        }
    }

    let next_gen = gen + 1;

    // Cull everyone not surviving.
    {
        let conn = db.conn.lock().unwrap();
        if survivors.is_empty() {
            // Nothing voted — keep everyone alive, just advance the gen.
        } else {
            let placeholders = survivors.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "UPDATE sheep SET alive = 0 WHERE alive = 1 AND id NOT IN ({placeholders})"
            );
            let params: Vec<&dyn rusqlite::ToSql> =
                survivors.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
            conn.execute(&sql, params.as_slice())?;
        }
    }

    // Breed survivors pairwise into children for the next gen.
    if survivors.len() >= 2 {
        for pair in survivors.windows(2) {
            let _ = breed_pair(db, &pair[0], &pair[1], next_gen, cfg);
        }
        // Also breed first + last to add variety.
        if survivors.len() >= 3 {
            let _ = breed_pair(db, &survivors[0], &survivors[survivors.len() - 1], next_gen, cfg);
        }
    }

    // Inject fresh-blood immigrants.
    for i in 0..cfg.immigrants {
        let mut rng = Rng::new(now_ms().wrapping_add(i as u64 * 104729));
        let genome = Genome::random(&mut rng, 3 + (i as usize % 3));
        let name = format!("Immigrant g{next_gen}.{i}");
        let _ = insert_sheep(db, &genome, &name, None, next_gen);
    }

    // Advance the generation clock.
    {
        let conn = db.conn.lock().unwrap();
        conn.execute(
            "UPDATE meta SET gen = ?1, gen_started_ms = ?2 WHERE id = 0",
            rusqlite::params![next_gen, now_ms() as i64],
        )?;
    }

    Ok(next_gen)
}
