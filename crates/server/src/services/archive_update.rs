//! Backs the console's "Force update: Storm Tracks" / "Force update: Recon
//! MET" / "Force update: TDR" buttons — runs the already-ported
//! `storms::run_ingest` / `recon_ingest::run_ingest` / `tdr_ingest::run_ingest`
//! as a detached background job and exposes its status for polling, the same
//! job-state pattern as `self_update`.
//!
//! "Force" refers only to bypassing the *console's own* re-click guard
//! (`is_running`) — it does not mean "reprocess everything". All three ingest
//! functions are already incremental: `recon_ingest::harvest_mission` skips a
//! mission whose `nc_version` hasn't changed, `storms::ingest_atcf_season`
//! only fetches seasons after `max_year_for_basin`, and `tdr_ingest` skips a
//! mission already indexed at the requested level unless forced. This module
//! always calls them with `force: false` and (for recon/tdr) `years: None`,
//! matching each CLI's default (`ingest-recon`/`ingest-tdr` with no
//! `--force`/`--full`); a full reprocess or deep historical backfill is only
//! ever a deliberate CLI run, never a console click.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use serde_json::{json, Value};

use crate::config::Paths;

pub const IN_PROGRESS_STATUSES: &[&str] = &["running"];

fn idle_job() -> Value {
    json!({
        "status": "idle", "started_at": Value::Null, "finished_at": Value::Null,
        "summary": Value::Null, "error": Value::Null,
    })
}

/// Shared archive-update job state, held in `AppState`. One slot per archive
/// (`storms`, `recon_met`, `tdr`) so updates can run independently.
pub struct ArchiveUpdateState {
    storms: Mutex<Value>,
    recon_met: Mutex<Value>,
    tdr: Mutex<Value>,
}

impl Default for ArchiveUpdateState {
    fn default() -> Self {
        Self {
            storms: Mutex::new(idle_job()),
            recon_met: Mutex::new(idle_job()),
            tdr: Mutex::new(idle_job()),
        }
    }
}

impl ArchiveUpdateState {
    fn slot(&self, archive: &str) -> Option<&Mutex<Value>> {
        match archive {
            "storms" => Some(&self.storms),
            "recon_met" => Some(&self.recon_met),
            "tdr" => Some(&self.tdr),
            _ => None,
        }
    }

    fn is_known_archive(archive: &str) -> bool {
        matches!(archive, "storms" | "recon_met" | "tdr")
    }

    pub fn job(&self, archive: &str) -> Option<Value> {
        self.slot(archive).map(|m| m.lock().unwrap().clone())
    }

    pub fn is_running(&self, archive: &str) -> bool {
        self.slot(archive)
            .map(|m| IN_PROGRESS_STATUSES.contains(&m.lock().unwrap()["status"].as_str().unwrap_or("")))
            .unwrap_or(false)
    }

    /// Marks the job "running" and returns its snapshot for the response body.
    fn start(&self, archive: &str) -> Value {
        let Some(m) = self.slot(archive) else { return idle_job() };
        let mut j = m.lock().unwrap();
        *j = json!({
            "status": "running", "started_at": Utc::now().to_rfc3339(),
            "finished_at": Value::Null, "summary": Value::Null, "error": Value::Null,
        });
        j.clone()
    }

    fn finish(&self, archive: &str, result: Result<Value, String>) {
        let Some(m) = self.slot(archive) else { return };
        let mut j = m.lock().unwrap();
        match result {
            Ok(summary) => {
                j["status"] = json!("done");
                j["summary"] = summary;
            }
            Err(e) => {
                j["status"] = json!("error");
                j["error"] = json!(e);
            }
        }
        j["finished_at"] = json!(Utc::now().to_rfc3339());
    }
}

/// Starts the named archive's update job (if not already running) and returns
/// its freshly-"running" snapshot. `None` for an unknown archive name.
///
/// `years` lets a console click reach further back than the default
/// shallow (current-1, current) window for the `tdr` archive specifically —
/// see the module doc comment on why this is still opt-in, deliberate, and
/// separate from the plain "force update" button. Ignored for `storms`
/// (which has no year concept) and `recon_met` (not exposed in the console
/// UI yet, though `recon_ingest::run_ingest` does support it).
///
/// Runs via `spawn_blocking` + `Handle::block_on` rather than a plain
/// `tokio::spawn`: both `storms::run_ingest` and `recon_ingest::run_ingest`
/// hold a `&rusqlite::Connection` (not `Sync`) across internal `.await`
/// points, so the futures they return aren't `Send` — fine for the CLI path,
/// which awaits them directly on the current task, but `tokio::spawn` needs
/// `Send` because it may hand the future to a different worker thread.
/// Driving the future to completion on one dedicated blocking-pool thread
/// sidesteps that requirement entirely.
pub fn start(
    state: &Arc<ArchiveUpdateState>,
    paths: &Arc<Paths>,
    archive: &str,
    years: Option<Vec<i64>>,
) -> Option<Value> {
    if !ArchiveUpdateState::is_known_archive(archive) {
        return None;
    }
    let snapshot = state.start(archive);
    match archive {
        "storms" => {
            let state = state.clone();
            let storms_db = paths.storms_db.clone();
            tokio::task::spawn_blocking(move || {
                tokio::runtime::Handle::current().block_on(run_storms(state, storms_db));
            });
        }
        "recon_met" => {
            let state = state.clone();
            let recon_db = paths.recon_met_db.clone();
            let storms_db = paths.storms_db.clone();
            tokio::task::spawn_blocking(move || {
                tokio::runtime::Handle::current().block_on(run_recon(state, recon_db, storms_db));
            });
        }
        "tdr" => {
            let state = state.clone();
            let tdr_db = paths.tdr_db.clone();
            tokio::task::spawn_blocking(move || {
                tokio::runtime::Handle::current().block_on(run_tdr(state, tdr_db, years));
            });
        }
        _ => unreachable!(),
    }
    Some(snapshot)
}

async fn run_storms(state: Arc<ArchiveUpdateState>, storms_db: PathBuf) {
    let result = crate::services::storms::run_ingest(&storms_db).await.map_err(|e| e.to_string());
    state.finish("storms", result);
}

async fn run_recon(state: Arc<ArchiveUpdateState>, recon_db: PathBuf, storms_db: PathBuf) {
    let result = crate::services::recon_ingest::run_ingest(&recon_db, &storms_db, None, false)
        .await
        .map_err(|e| e.to_string());
    state.finish("recon_met", result);
}

/// `years: None` defaults to [current-1, current] — same shallow-ingest
/// convention as the CLI's `ingest-tdr` with no `--years`/`--full`. Passing
/// an explicit `years` (from the console's backfill control) reaches further
/// back, same as `ingest-tdr --years`. `force` is always `false` here —
/// still just "run it now", not "reprocess everything already indexed".
async fn run_tdr(state: Arc<ArchiveUpdateState>, tdr_db: PathBuf, years: Option<Vec<i64>>) {
    let result = crate::services::tdr_ingest::run_ingest(&tdr_db, years, false)
        .await
        .map_err(|e| e.to_string());
    state.finish("tdr", result);
}
