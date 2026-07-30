//! Backs the console's "Force update: Storm Tracks" / "Force update: Recon
//! MET" / "Force update: TDR" buttons — runs the already-ported
//! `storms::run_ingest` / `recon_ingest::run_ingest` / `tdr_ingest::run_ingest`
//! as a detached background job and exposes its status for polling, the same
//! job-state pattern as `self_update`.
//!
//! Each running job also publishes a live `progress` block (see
//! `services/progress.rs`) alongside its status, so the console can show which
//! phase the ingest is in and when it last moved rather than a bare "running"
//! that looks identical to a hang.
//!
//! "Force" on the storm-tracks/recon-MET buttons refers only to bypassing the
//! *console's own* re-click guard (`is_running`) — it does not mean
//! "reprocess everything". Those two ingest functions are already
//! incremental (`recon_ingest::harvest_mission` skips a mission whose
//! `nc_version` hasn't changed; `storms::ingest_atcf_season` only fetches
//! seasons after `max_year_for_basin`) and this module always calls them with
//! `force: false`, matching `ingest-recon`'s CLI default.
//!
//! TDR is the one archive where a real, deliberate `--force` (re-crawl a
//! mission already indexed, e.g. to re-derive `storm_name` after a parsing
//! fix) is exposed all the way through to the console's "Force update: TDR" /
//! "Backfill TDR years" buttons via an explicit checkbox — see `run_tdr` and
//! `routers/admin.rs::start_archive_update`'s `force` query param. It's
//! opt-in and off by default (`false` unless the checkbox was ticked) because
//! it's a materially heavier operation: it re-fetches every already-indexed
//! mission's file listing (and, for Level 1b, its jobfile) instead of only
//! new ones.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use serde_json::{json, Value};

use crate::config::Paths;
use crate::services::progress::Progress;

pub const IN_PROGRESS_STATUSES: &[&str] = &["running"];

fn idle_job() -> Value {
    json!({
        "status": "idle", "started_at": Value::Null, "finished_at": Value::Null,
        "summary": Value::Null, "error": Value::Null,
    })
}

/// One archive's job record: the terminal-state JSON plus the live [`Progress`]
/// the running ingest writes into. They're separate because they're written at
/// different rates — `job` changes three times per run, `progress` changes
/// hundreds of times a second — and only `job` needs to survive the run.
#[derive(Default)]
struct Slot {
    job: Mutex<Value>,
    progress: Progress,
}

/// Shared archive-update job state, held in `AppState`. One slot per archive
/// (`storms`, `recon_met`, `tdr`) so updates can run independently.
pub struct ArchiveUpdateState {
    storms: Slot,
    recon_met: Slot,
    tdr: Slot,
}

impl Default for ArchiveUpdateState {
    fn default() -> Self {
        Self {
            storms: Slot { job: Mutex::new(idle_job()), progress: Progress::default() },
            recon_met: Slot { job: Mutex::new(idle_job()), progress: Progress::default() },
            tdr: Slot { job: Mutex::new(idle_job()), progress: Progress::default() },
        }
    }
}

impl ArchiveUpdateState {
    fn slot(&self, archive: &str) -> Option<&Slot> {
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

    /// The job record, with the live progress report folded in while the job
    /// is running. Progress is dropped from a finished job on purpose: once
    /// there's a summary, a stale "12/47 missions" line is only misleading.
    pub fn job(&self, archive: &str) -> Option<Value> {
        let slot = self.slot(archive)?;
        let mut job = slot.job.lock().unwrap().clone();
        if IN_PROGRESS_STATUSES.contains(&job["status"].as_str().unwrap_or("")) {
            job["progress"] = serde_json::to_value(slot.progress.snapshot()).unwrap_or(Value::Null);
        }
        Some(job)
    }

    pub fn is_running(&self, archive: &str) -> bool {
        self.slot(archive)
            .map(|s| {
                IN_PROGRESS_STATUSES.contains(&s.job.lock().unwrap()["status"].as_str().unwrap_or(""))
            })
            .unwrap_or(false)
    }

    /// Marks the job "running" and returns its snapshot for the response body.
    fn start(&self, archive: &str) -> Value {
        let Some(slot) = self.slot(archive) else { return idle_job() };
        slot.progress.phase("Starting", None);
        let mut j = slot.job.lock().unwrap();
        *j = json!({
            "status": "running", "started_at": Utc::now().to_rfc3339(),
            "finished_at": Value::Null, "summary": Value::Null, "error": Value::Null,
        });
        j.clone()
    }

    /// Marks a still-"running" job as failed. No-op once the job has finished
    /// on its own — the normal path is `finish`, called from inside the job.
    fn fail_if_running(&self, archive: &str, error: String) {
        let Some(slot) = self.slot(archive) else { return };
        let mut j = slot.job.lock().unwrap();
        if IN_PROGRESS_STATUSES.contains(&j["status"].as_str().unwrap_or("")) {
            j["status"] = json!("error");
            j["error"] = json!(error);
            j["finished_at"] = json!(Utc::now().to_rfc3339());
        }
    }

    fn finish(&self, archive: &str, result: Result<Value, String>) {
        let Some(slot) = self.slot(archive) else { return };
        let mut j = slot.job.lock().unwrap();
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
/// `force` is a genuine "reprocess already-indexed missions" flag, but only
/// `tdr` acts on it — see the module doc comment. Ignored for `storms`/
/// `recon_met`, which stay incremental-only from the console.
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
    force: bool,
) -> Option<Value> {
    if !ArchiveUpdateState::is_known_archive(archive) {
        return None;
    }
    let snapshot = state.start(archive);
    // Cloned out of the slot up front so the job owns a handle to the very
    // cell `job()` reads, without holding a borrow on `state`.
    let progress = state.slot(archive).map(|s| s.progress.clone()).unwrap_or_default();
    match archive {
        "storms" => {
            let job_state = state.clone();
            let storms_db = paths.storms_db.clone();
            let handle = tokio::task::spawn_blocking(move || {
                tokio::runtime::Handle::current().block_on(run_storms(job_state, storms_db, progress));
            });
            supervise(state.clone(), "storms", handle);
        }
        "recon_met" => {
            let job_state = state.clone();
            let recon_db = paths.recon_met_db.clone();
            let storms_db = paths.storms_db.clone();
            let handle = tokio::task::spawn_blocking(move || {
                tokio::runtime::Handle::current()
                    .block_on(run_recon(job_state, recon_db, storms_db, progress));
            });
            supervise(state.clone(), "recon_met", handle);
        }
        "tdr" => {
            let job_state = state.clone();
            let tdr_db = paths.tdr_db.clone();
            let handle = tokio::task::spawn_blocking(move || {
                tokio::runtime::Handle::current()
                    .block_on(run_tdr(job_state, tdr_db, years, force, progress));
            });
            supervise(state.clone(), "tdr", handle);
        }
        _ => unreachable!(),
    }
    Some(snapshot)
}

/// Watches the blocking job's `JoinHandle` so a panic inside the ingest can't
/// strand the slot at "running". The console polls a running job every 3s with
/// no timeout and no way to clear it, so a crashed job is indistinguishable
/// from one that simply never ends — it just polls forever. Turning the panic
/// into an "error" status makes it show up in the console instead.
fn supervise(state: Arc<ArchiveUpdateState>, archive: &'static str, handle: tokio::task::JoinHandle<()>) {
    tokio::spawn(async move {
        if let Err(e) = handle.await {
            tracing::error!("archive update '{archive}' crashed: {e}");
            state.fail_if_running(archive, format!("update job crashed: {e}"));
        }
    });
}

async fn run_storms(state: Arc<ArchiveUpdateState>, storms_db: PathBuf, progress: Progress) {
    let result =
        crate::services::storms::run_ingest(&storms_db, &progress).await.map_err(|e| e.to_string());
    state.finish("storms", result);
}

async fn run_recon(
    state: Arc<ArchiveUpdateState>,
    recon_db: PathBuf,
    storms_db: PathBuf,
    progress: Progress,
) {
    let result = crate::services::recon_ingest::run_ingest(&recon_db, &storms_db, None, false, &progress)
        .await
        .map_err(|e| e.to_string());
    state.finish("recon_met", result);
}

/// `years: None` defaults to [current-1, current] — same shallow-ingest
/// convention as the CLI's `ingest-tdr` with no `--years`/`--full`. Passing
/// an explicit `years` (from the console's backfill control) reaches further
/// back, same as `ingest-tdr --years`. `force`, when the console's "Force
/// re-crawl" checkbox was ticked, re-fetches every already-indexed mission
/// in scope instead of only new ones — same as `ingest-tdr --force` — so a
/// storm-name parsing fix (or a manual correction that's since been
/// un-locked) can actually be re-applied without a CLI session. A locked
/// mission (`tdr::edit_mission`) is still never touched, force or not.
async fn run_tdr(
    state: Arc<ArchiveUpdateState>,
    tdr_db: PathBuf,
    years: Option<Vec<i64>>,
    force: bool,
    progress: Progress,
) {
    let result = crate::services::tdr_ingest::run_ingest(&tdr_db, years, force, &progress)
        .await
        .map_err(|e| e.to_string());
    state.finish("tdr", result);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Puts a slot in the state a running job leaves it in, without spawning
    /// the ingest: "running" plus a progress cell the job has written to.
    fn running_with_progress(archive: &str) -> ArchiveUpdateState {
        let state = ArchiveUpdateState::default();
        state.start(archive);
        let slot = state.slot(archive).unwrap();
        slot.progress.phase("ATCF AL 2026", Some(47));
        slot.progress.detail("bal012026.dat");
        slot.progress.step();
        state
    }

    #[test]
    fn running_job_carries_live_progress() {
        let state = running_with_progress("storms");
        let job = state.job("storms").unwrap();
        assert_eq!(job["status"], "running");
        assert_eq!(job["progress"]["phase"], "ATCF AL 2026");
        assert_eq!(job["progress"]["detail"], "bal012026.dat");
        assert_eq!(job["progress"]["done"], 1);
        assert_eq!(job["progress"]["total"], 47);
        assert!(job["progress"]["updated_at"].is_string());
    }

    #[test]
    fn finished_job_drops_stale_progress() {
        let state = running_with_progress("storms");
        state.finish("storms", Ok(json!({ "total_storms": 1925 })));
        let job = state.job("storms").unwrap();
        assert_eq!(job["status"], "done");
        // A summary is on the record now; "1/47" would only mislead.
        assert!(job.get("progress").is_none());
    }

    #[test]
    fn idle_job_has_no_progress() {
        let state = ArchiveUpdateState::default();
        assert!(state.job("storms").unwrap().get("progress").is_none());
    }

    #[test]
    fn slots_are_independent() {
        let state = running_with_progress("tdr");
        assert!(state.is_running("tdr"));
        assert!(!state.is_running("storms"));
        assert!(state.job("storms").unwrap().get("progress").is_none());
    }

    #[test]
    fn fail_if_running_rescues_a_stranded_job() {
        let state = running_with_progress("recon_met");
        state.fail_if_running("recon_met", "update job crashed: panic".into());
        let job = state.job("recon_met").unwrap();
        assert_eq!(job["status"], "error");
        assert_eq!(job["error"], "update job crashed: panic");
        assert!(job["finished_at"].is_string());
    }

    #[test]
    fn fail_if_running_never_overwrites_a_finished_job() {
        // The supervisor and the job itself race by construction: the handle
        // resolves right after `finish` returns. A successful run must win.
        let state = running_with_progress("storms");
        state.finish("storms", Ok(json!({ "total_storms": 1925 })));
        state.fail_if_running("storms", "update job crashed: panic".into());
        let job = state.job("storms").unwrap();
        assert_eq!(job["status"], "done");
        assert_eq!(job["error"], Value::Null);
    }

    #[test]
    fn unknown_archive_has_no_slot() {
        let state = ArchiveUpdateState::default();
        assert!(state.job("goes").is_none());
        assert!(!state.is_running("goes"));
        assert!(!ArchiveUpdateState::is_known_archive("goes"));
    }
}
