//! Backs the console's "Force update: Storm Tracks" / "Force update: Recon
//! MET" buttons — runs the already-ported `storms::run_ingest` /
//! `recon_ingest::run_ingest` as a detached background job and exposes its
//! status for polling, the same job-state pattern as `self_update`.
//!
//! "Force" refers only to bypassing the *console's own* re-click guard
//! (`is_running`) — it does not mean "reprocess everything". Both ingest
//! functions are already incremental: `recon_ingest::harvest_mission` skips a
//! mission whose `nc_version` hasn't changed, and `storms::ingest_atcf_season`
//! only fetches seasons after `max_year_for_basin`. This module always calls
//! them with `force: false`, matching the CLI's default (`ingest-recon`
//! without `--force`); a full reprocess is only ever a deliberate `--force`
//! CLI run, never a console click.

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
/// (`storms`, `recon_met`) so the two updates can run independently.
pub struct ArchiveUpdateState {
    storms: Mutex<Value>,
    recon_met: Mutex<Value>,
}

impl Default for ArchiveUpdateState {
    fn default() -> Self {
        Self { storms: Mutex::new(idle_job()), recon_met: Mutex::new(idle_job()) }
    }
}

impl ArchiveUpdateState {
    fn slot(&self, archive: &str) -> Option<&Mutex<Value>> {
        match archive {
            "storms" => Some(&self.storms),
            "recon_met" => Some(&self.recon_met),
            _ => None,
        }
    }

    fn is_known_archive(archive: &str) -> bool {
        matches!(archive, "storms" | "recon_met")
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
/// Runs via `spawn_blocking` + `Handle::block_on` rather than a plain
/// `tokio::spawn`: both `storms::run_ingest` and `recon_ingest::run_ingest`
/// hold a `&rusqlite::Connection` (not `Sync`) across internal `.await`
/// points, so the futures they return aren't `Send` — fine for the CLI path,
/// which awaits them directly on the current task, but `tokio::spawn` needs
/// `Send` because it may hand the future to a different worker thread.
/// Driving the future to completion on one dedicated blocking-pool thread
/// sidesteps that requirement entirely.
pub fn start(state: &Arc<ArchiveUpdateState>, paths: &Arc<Paths>, archive: &str) -> Option<Value> {
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
