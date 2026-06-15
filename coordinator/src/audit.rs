//! Peer-offloaded auditing: the machinery that lets the coordinator NOT
//! re-render tiles on the happy path.
//!
//! THE MODEL (see ARCHITECTURE.md §4 "Peer-offloaded auditing"):
//!
//! 1. **Ingest is cheap.** `/submit` content-hashes the uploaded pixels, merges
//!    them, and records the tile as `unaudited` with the submitter + content
//!    hash. NO render happens here. (`routes::post_submit`.)
//!
//! 2. **`/assign` plants audit tasks.** A requester's bundle carries `audits`:
//!    a reputation-weighted sample of OTHER contributors' `unaudited` tiles
//!    (new/low-rep submitters audited heavily, trusted ones lightly), plus the
//!    occasional **honeypot** — a tile the coordinator already knows the answer
//!    to, so it can grade the auditor for free. (`sample_audits`.)
//!
//! 3. **`/submit` grades audit reports.** The auditor re-rendered each task and
//!    reports a hash. (`grade_report`.)
//!      - real audit, hash matches stored tile hash  → validate the tile, bump
//!        both reputations.
//!      - real audit, hash mismatch                  → DISPUTE: the coordinator
//!        re-renders THAT ONE tile (the only happy-path-adjacent render, rare)
//!        to find ground truth, bans the liar, and `disk`-subtracts a fraudulent
//!        submitter's merged contribution.
//!      - honeypot, wrong answer                     → the auditor is lazy /
//!        colluding → penalize/ban the auditor.
//!
//! 4. **Reputation** governs (a) the per-submitter sample rate and (b) how much
//!    an auditor's verdict counts: a non-honeypot mismatch from a low-trust
//!    auditor needs corroboration (≥1 trusted OR ≥2 independent auditors) before
//!    it triggers a dispute, so a lone griefer-auditor can't frame an honest
//!    submitter for free.

use rusqlite::Connection;
use serde_json::{json, Value};

use crate::ga::now_ms;
use crate::spec;

/// Reputation at/above which a key is "trusted": its tiles are only lightly
/// sampled and its lone audit verdict is enough to trigger a dispute.
pub const TRUST_REP: f64 = 5.0;

/// Floor sample rate — even a fully trusted key keeps a fraction of its tiles
/// audited (never 0%, per the architecture). New keys are ~fully audited.
pub const SAMPLE_FLOOR: f64 = 0.05;

/// How many audit tasks to pack into one `/assign` bundle, at most.
pub const AUDITS_PER_BUNDLE: usize = 4;

/// Default: roughly one in this many bundles carries a honeypot (free grading /
/// catches rubber-stampers). Overridable with the `HONEYPOT_EVERY` env var; 0
/// disables honeypots (used by tests that assert the verify-render counter, and
/// available as an operational kill-switch).
pub const HONEYPOT_EVERY_DEFAULT: u64 = 4;

fn honeypot_every() -> u64 {
    std::env::var("HONEYPOT_EVERY")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(HONEYPOT_EVERY_DEFAULT)
}

/// Probability (0..1) that a given unaudited tile from a submitter with this
/// reputation should be audited. New/zero-rep keys → 1.0 (~fully audited);
/// decays toward `SAMPLE_FLOOR` as accepted, validated work accumulates.
pub fn sample_rate(reputation: f64) -> f64 {
    if reputation <= 0.0 {
        return 1.0;
    }
    // Smooth decay: rate = max(floor, TRUST_REP / (TRUST_REP + reputation)).
    let r = TRUST_REP / (TRUST_REP + reputation);
    r.max(SAMPLE_FLOOR)
}

/// Build the `audits` array for an `/assign` bundle: a reputation-weighted
/// sample of OTHER contributors' `unaudited` tiles, never the requester's own,
/// prioritizing low-reputation/new submitters; mix in an occasional honeypot.
///
/// Each returned audit task is a full WorkUnit (genome/frame/idx/spec) WITHOUT
/// revealing the submitter's claimed hash. We record the planted task in the
/// `audit` ledger so the later report can be graded.
pub fn sample_audits(conn: &Connection, requester: &str) -> Vec<Value> {
    let mut out = Vec::new();

    // Candidate unaudited tiles from OTHER submitters, joined to their account
    // reputation, lowest-reputation submitters first (heaviest scrutiny), then
    // oldest unaudited. We over-fetch and then sample by rate.
    let candidates: Vec<(String, u32, u32, String, f64)> = {
        let mut stmt = match conn.prepare(
            "SELECT t.sheep_id, t.frame, t.idx, t.pub,
                    COALESCE(a.reputation, 0.0) AS rep
             FROM tile t
             LEFT JOIN account a ON a.pub = t.pub
             WHERE t.status = 1 AND t.audit_status = 0
               AND t.pub IS NOT NULL AND t.pub != ?1
               AND NOT EXISTS (
                   SELECT 1 FROM audit au
                   WHERE au.sheep_id = t.sheep_id AND au.frame = t.frame
                     AND au.idx = t.idx AND au.status = 0
               )
             ORDER BY rep ASC, t.assigned_ms ASC
             LIMIT 64",
        ) {
            Ok(s) => s,
            Err(_) => return out,
        };
        let rows = stmt.query_map([requester], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)? as u32,
                r.get::<_, i64>(2)? as u32,
                r.get::<_, String>(3)?,
                r.get::<_, f64>(4)?,
            ))
        });
        match rows {
            Ok(rows) => rows.filter_map(Result::ok).collect(),
            Err(_) => return out,
        }
    };

    let mut planted = 0usize;
    let now = now_ms();
    for (i, (sheep_id, frame, idx, submitter, rep)) in candidates.into_iter().enumerate() {
        if planted >= AUDITS_PER_BUNDLE {
            break;
        }
        // Reputation-weighted sampling: pseudo-random gate by sample_rate.
        // Use a cheap mix of the wall clock + position so it varies per bundle.
        let gate = ((now.wrapping_add(i as u64).wrapping_mul(2654435761)) % 1000) as f64 / 1000.0;
        if gate >= sample_rate(rep) {
            continue;
        }

        let Some(genome_json) = sheep_genome(conn, &sheep_id) else { continue };

        // Record the planted real audit task.
        if conn
            .execute(
                "INSERT INTO audit (auditor, sheep_id, frame, idx, kind, submitter, status, created_ms)
                 VALUES (?1, ?2, ?3, ?4, 0, ?5, 0, ?6)",
                rusqlite::params![requester, sheep_id, frame, idx, submitter, now as i64],
            )
            .is_err()
        {
            continue;
        }
        out.push(work_unit(&sheep_id, &genome_json, frame, idx));
        planted += 1;
    }

    // Mix in a honeypot occasionally: a planted task whose correct hash the
    // coordinator computes ONCE here, grading the auditor's later report for free
    // (catches rubber-stampers who report without actually re-rendering).
    let hp_every = honeypot_every();
    let force = std::env::var("HONEYPOT_FORCE").map(|v| v == "1").unwrap_or(false);
    let plant = force || (!out.is_empty() && hp_every > 0 && (now % hp_every == 0));
    if plant {
        if let Some(hp) = plant_honeypot(conn, requester, now) {
            out.push(hp);
        }
    }

    out
}

/// Plant a honeypot: pick a known living sheep + an arbitrary (frame, idx),
/// compute the correct hash once, store it as the expected answer, and hand the
/// auditor the WorkUnit. Indistinguishable from a real audit on the wire.
fn plant_honeypot(conn: &Connection, requester: &str, now: u64) -> Option<Value> {
    let (sheep_id, genome_json): (String, String) = conn
        .query_row(
            "SELECT id, genome FROM sheep WHERE alive = 1 ORDER BY tiles DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok()?;
    // A deterministic-but-varied (frame, idx) within spec bounds.
    let frame = (now % spec::N_FRAMES as u64) as u32;
    let idx = ((now / 7) % spec::IDXS_PER_FRAME as u64) as u32;

    let genome = crate::render::parse_genome(&genome_json).ok()?;
    let sid = crate::render::sheep_id_bytes(&sheep_id).ok()?;
    let expected = crate::render::verify_tile_hash(
        &genome, &sid, frame, idx, spec::W, spec::H, spec::SS, spec::SPP, spec::N_FRAMES,
    );

    conn.execute(
        "INSERT INTO audit (auditor, sheep_id, frame, idx, kind, expected_hash, status, created_ms)
         VALUES (?1, ?2, ?3, ?4, 1, ?5, 0, ?6)",
        rusqlite::params![requester, sheep_id, frame, idx, expected, now as i64],
    )
    .ok()?;
    Some(work_unit(&sheep_id, &genome_json, frame, idx))
}

/// Load a sheep's genome JSON, if it exists.
fn sheep_genome(conn: &Connection, sheep_id: &str) -> Option<String> {
    conn.query_row("SELECT genome FROM sheep WHERE id = ?1", [sheep_id], |r| r.get(0))
        .ok()
}

/// A WorkUnit JSON object (same shape `/assign` hands out for render work) — it
/// names exactly the args of `render_batch`. The submitter's claimed hash is
/// deliberately NOT included.
fn work_unit(sheep_id: &str, genome_json: &str, frame: u32, idx: u32) -> Value {
    json!({
        "sheepId": sheep_id,
        "genomeJson": genome_json,
        "frame": frame,
        "idx": idx,
        "w": spec::W,
        "h": spec::H,
        "ss": spec::SS,
        "spp": spec::SPP,
        "nFrames": spec::N_FRAMES,
    })
}

/// The outcome of grading one audit report — surfaced so `/submit` can apply
/// side effects (dispute re-render, bans, contribution subtraction) outside the
/// DB lock.
#[derive(Debug, Default)]
pub struct GradeOutcome {
    /// Tiles that became validated (audited) this report — for stats/logging.
    pub validated: u32,
    /// A real-audit hash mismatch that cleared the corroboration bar: the
    /// coordinator must now re-render this ONE tile to find ground truth.
    /// `(sheep_id, frame, idx, submitter, auditor, auditor_hash, stored_hash)`.
    pub disputes: Vec<Dispute>,
}

#[derive(Debug)]
pub struct Dispute {
    pub sheep_id: String,
    pub frame: u32,
    pub idx: u32,
    pub submitter: String,
    pub auditor: String,
    pub auditor_hash: String,
    pub stored_hash: String,
}

/// Grade ALL of an auditor's reported hashes against their pending planted
/// audit tasks. Runs entirely under the DB lock (cheap: comparisons + counter
/// bumps); collects DISPUTES (which need a re-render) for the caller to resolve
/// after dropping the lock.
///
/// `reports` is the `audit_reports` array: `[{ sheepId, frame, idx, hash }]`.
pub fn grade_reports(
    conn: &Connection,
    auditor: &str,
    reports: &[Value],
) -> GradeOutcome {
    let mut outcome = GradeOutcome::default();

    for rep in reports {
        let sheep_id = rep.get("sheepId").and_then(Value::as_str).unwrap_or("");
        let frame = rep.get("frame").and_then(Value::as_u64).unwrap_or(u64::MAX) as u32;
        let idx = rep.get("idx").and_then(Value::as_u64).unwrap_or(u64::MAX) as u32;
        let hash = rep
            .get("hash")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if sheep_id.is_empty() || hash.is_empty() {
            continue;
        }

        // Find this auditor's pending planted task for this tile.
        let task: Option<(i64, i64, Option<String>, Option<String>)> = conn
            .query_row(
                "SELECT id, kind, expected_hash, submitter FROM audit
                 WHERE auditor = ?1 AND sheep_id = ?2 AND frame = ?3 AND idx = ?4
                   AND status = 0
                 ORDER BY id ASC LIMIT 1",
                rusqlite::params![auditor, sheep_id, frame, idx],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .ok();
        let Some((audit_id, kind, expected_hash, submitter)) = task else {
            // No pending task matches — ignore (already graded / never assigned).
            continue;
        };

        // Mark the task resolved regardless of verdict.
        let _ = conn.execute("UPDATE audit SET status = 1 WHERE id = ?1", [audit_id]);
        let _ = conn.execute(
            "UPDATE account SET audits_done = audits_done + 1 WHERE pub = ?1",
            [auditor],
        );

        if kind == 1 {
            // HONEYPOT: grade against the known answer (free — no re-render).
            let correct = expected_hash.as_deref() == Some(hash.as_str());
            if correct {
                let _ = conn.execute(
                    "UPDATE account SET audits_correct = audits_correct + 1,
                                        reputation = reputation + 0.05 WHERE pub = ?1",
                    [auditor],
                );
            } else {
                // Wrong on a honeypot ⇒ the auditor did not actually re-render
                // (rubber-stamper / colluder). Penalize, and ban on a blatant
                // wrong answer (we know the truth for free).
                let _ = conn.execute(
                    "UPDATE account SET reputation = MAX(reputation - 1.0, -10.0),
                                        banned = 1, credits = 0 WHERE pub = ?1",
                    [auditor],
                );
                let _ = conn.execute(
                    "UPDATE tile SET status = 2, pub = NULL WHERE pub = ?1",
                    [auditor],
                );
            }
            continue;
        }

        // REAL AUDIT: compare against the tile's stored content hash.
        let stored: Option<(Option<String>, i64)> = conn
            .query_row(
                "SELECT hash, audit_status FROM tile
                 WHERE sheep_id = ?1 AND frame = ?2 AND idx = ?3 AND status = 1",
                rusqlite::params![sheep_id, frame, idx],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        let Some((Some(stored_hash), _audit_status)) = stored else {
            continue; // tile vanished (e.g. submitter already banned) — skip
        };

        if hash == stored_hash {
            // MATCH: the peer re-render confirms the trust-ingested pixels.
            // Validate the tile; bump submitter + auditor reputations.
            let _ = conn.execute(
                "UPDATE tile SET audit_status = 1
                 WHERE sheep_id = ?1 AND frame = ?2 AND idx = ?3",
                rusqlite::params![sheep_id, frame, idx],
            );
            let _ = conn.execute(
                "UPDATE account SET audits_correct = audits_correct + 1,
                                    reputation = reputation + 0.05 WHERE pub = ?1",
                [auditor],
            );
            if let Some(sub) = &submitter {
                let _ = conn.execute(
                    "UPDATE account SET reputation = reputation + 0.1 WHERE pub = ?1",
                    [sub],
                );
            }
            outcome.validated += 1;
        } else {
            // MISMATCH: the auditor's re-render disagrees with the stored pixels.
            // One of them is lying. Require corroboration before we spend a
            // dispute re-render: a TRUSTED auditor's lone verdict is enough; an
            // untrusted auditor needs a second independent disagreement.
            let auditor_rep: f64 = conn
                .query_row(
                    "SELECT reputation FROM account WHERE pub = ?1",
                    [auditor],
                    |r| r.get(0),
                )
                .unwrap_or(0.0);

            let corroborated = auditor_rep >= TRUST_REP || {
                // Count other RESOLVED real audits on this same tile by other
                // auditors (any disagreeing report would have left a record /
                // this auditor is the Nth) — simplest real signal: number of
                // distinct auditors who have been assigned this tile.
                let others: i64 = conn
                    .query_row(
                        "SELECT COUNT(DISTINCT auditor) FROM audit
                         WHERE sheep_id = ?1 AND frame = ?2 AND idx = ?3
                           AND kind = 0 AND auditor != ?4",
                        rusqlite::params![sheep_id, frame, idx, auditor],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                others >= 1
            };

            if corroborated {
                let submitter = submitter.unwrap_or_default();
                outcome.disputes.push(Dispute {
                    sheep_id: sheep_id.to_string(),
                    frame,
                    idx,
                    submitter,
                    auditor: auditor.to_string(),
                    auditor_hash: hash,
                    stored_hash,
                });
            } else {
                // Hold: re-plant the tile for another auditor (don't validate,
                // don't dispute yet). Leaving audit_status=0 means /assign can
                // hand it to a second auditor for corroboration.
                tracing::info!(
                    "audit: uncorroborated mismatch on {}/{}/{} from low-rep auditor — \
                     awaiting a second opinion before dispute",
                    &sheep_id[..sheep_id.len().min(8)],
                    frame,
                    idx,
                );
            }
        }
    }

    outcome
}
