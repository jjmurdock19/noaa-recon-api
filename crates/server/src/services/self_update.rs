//! Self-update: pull the latest code from the git remote, rebuild if
//! `Cargo.lock` changed, then restart. Port of `app/services/self_update.py`.
//!
//! Runs entirely as the unprivileged service user (see
//! deploy/noaa-recon-api.service equivalent written by install.sh) — no
//! sudo, no root. The trick: that unit has Restart=on-failure, so
//! "restarting" is just deliberately exiting with a non-zero code and
//! letting systemd relaunch the binary, which picks up the freshly-pulled
//! (and, if needed, freshly-built) files on the next process start. There
//! is no in-place code reload.
//!
//! Safety: only ever a fast-forward pull (`git pull --ff-only`) of whatever
//! branch HEAD is currently tracking, and only when the working tree is
//! clean. Either check failing refuses the update with an explicit error
//! instead of merging or discarding anything — see [`apply_update`].
//!
//! Rust variant note: Python's `pip install -e .` step becomes
//! `cargo build --release -p noaa-recon-api` here, run only when
//! `Cargo.lock` changed by the pull (mirrors checking `pyproject.toml`).

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use chrono::Utc;
use serde_json::{json, Value};
use tokio::process::Command;
use tokio::time::timeout;

const REMOTE: &str = "origin";

/// In-progress job statuses — mirrors Python's `_SELF_UPDATE_IN_PROGRESS_STATUSES`.
pub const IN_PROGRESS_STATUSES: &[&str] = &["checking", "pulling", "installing_dependencies"];

/// Shared self-update state, held in `AppState`.
pub struct SelfUpdateState {
    pub job: Mutex<Value>,
    check: Mutex<Value>,
}

impl Default for SelfUpdateState {
    fn default() -> Self {
        Self {
            job: Mutex::new(json!({
                "status": "idle", "started_at": Value::Null, "finished_at": Value::Null,
                "result": Value::Null, "error": Value::Null, "new_commit": Value::Null,
            })),
            check: Mutex::new(json!({ "checked_at": Value::Null, "result": Value::Null, "error": Value::Null })),
        }
    }
}

impl SelfUpdateState {
    pub fn get_cached_check(&self) -> Value {
        self.check.lock().unwrap().clone()
    }

    pub fn set_cached_check(&self, result: Option<Value>, error: Option<String>) {
        let mut c = self.check.lock().unwrap();
        *c = json!({
            "checked_at": Utc::now().to_rfc3339(),
            "result": result,
            "error": error,
        });
    }

    pub fn job_status(&self) -> String {
        self.job.lock().unwrap()["status"].as_str().unwrap_or("").to_string()
    }
}

async fn git(repo_root: &Path, args: &[&str], timeout_secs: u64) -> Result<String, String> {
    let fut = Command::new("git").args(args).current_dir(repo_root).output();
    let output = timeout(Duration::from_secs(timeout_secs), fut)
        .await
        .map_err(|_| format!("git {} timed out", args.join(" ")))?
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let msg = if !stderr.is_empty() { stderr } else { stdout };
        return Err(if msg.is_empty() { format!("git {} failed", args.join(" ")) } else { msg });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

async fn current_branch(repo_root: &Path) -> Result<String, String> {
    git(repo_root, &["rev-parse", "--abbrev-ref", "HEAD"], 10).await
}

async fn working_tree_clean(repo_root: &Path) -> Result<bool, String> {
    Ok(git(repo_root, &["status", "--porcelain"], 15).await?.is_empty())
}

/// Fetch the remote and report whether the local branch is behind it.
/// Read-only — never pulls or modifies the working tree.
pub async fn check_for_update(repo_root: &Path) -> Result<Value, String> {
    let branch = current_branch(repo_root).await?;
    git(repo_root, &["fetch", REMOTE, &branch], 60).await?;
    let local = git(repo_root, &["rev-parse", "HEAD"], 10).await?;
    let remote_ref = format!("{REMOTE}/{branch}");
    let remote = git(repo_root, &["rev-parse", &remote_ref], 10).await?;
    if local == remote {
        return Ok(json!({
            "up_to_date": true, "local_commit": local, "remote_commit": remote,
            "commits_behind": 0, "log": [],
        }));
    }
    let range = format!("HEAD..{remote_ref}");
    let log = git(repo_root, &["log", "--oneline", &range], 15).await?;
    let lines: Vec<&str> = if log.is_empty() { Vec::new() } else { log.lines().collect() };
    Ok(json!({
        "up_to_date": false, "local_commit": local, "remote_commit": remote,
        "commits_behind": lines.len(), "log": lines,
    }))
}

fn set_status(job: &Mutex<Value>, status: &str) {
    job.lock().unwrap()["status"] = json!(status);
}

/// Pull + rebuild (if `Cargo.lock` changed), then exit so systemd restarts
/// the process on the new binary. Mutates the shared job state as it
/// progresses so the console can poll status the same way it already does
/// for prefetch/archive-update jobs. Intended to run as a detached
/// background task — never awaited by the request handler that starts it.
pub async fn apply_update(repo_root: PathBuf, state: std::sync::Arc<SelfUpdateState>) {
    let result: Result<(), String> = async {
        set_status(&state.job, "checking");
        let branch = current_branch(&repo_root).await?;
        git(&repo_root, &["fetch", REMOTE, &branch], 60).await?;
        let local_before = git(&repo_root, &["rev-parse", "HEAD"], 10).await?;
        let remote_ref = format!("{REMOTE}/{branch}");
        let remote = git(&repo_root, &["rev-parse", &remote_ref], 10).await?;
        if local_before == remote {
            let mut j = state.job.lock().unwrap();
            j["status"] = json!("up_to_date");
            j["result"] = json!("Already up to date.");
            return Ok(());
        }

        if !working_tree_clean(&repo_root).await? {
            return Err(
                "Working tree has uncommitted changes on the server — refusing to pull. \
                 Resolve manually (git status) before retrying."
                    .to_string(),
            );
        }

        set_status(&state.job, "pulling");
        let lock_path = repo_root.join("Cargo.lock");
        let old_lock = tokio::fs::read_to_string(&lock_path).await.unwrap_or_default();
        git(&repo_root, &["pull", "--ff-only", REMOTE, &branch], 60).await?;
        let new_commit = git(&repo_root, &["rev-parse", "HEAD"], 10).await?;
        let new_lock = tokio::fs::read_to_string(&lock_path).await.unwrap_or_default();

        if new_lock != old_lock {
            set_status(&state.job, "installing_dependencies");
            let fut = Command::new("cargo")
                .args(["build", "--release", "-p", "noaa-recon-api"])
                .current_dir(&repo_root)
                .output();
            let output = timeout(Duration::from_secs(1800), fut)
                .await
                .map_err(|_| "cargo build timed out".to_string())?
                .map_err(|e| e.to_string())?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let tail: String = stderr.chars().rev().take(4000).collect::<Vec<_>>().into_iter().rev().collect();
                return Err(format!("cargo build failed:\n{tail}"));
            }
        }

        let mut j = state.job.lock().unwrap();
        j["new_commit"] = json!(new_commit);
        j["result"] = json!(format!(
            "Updated {} -> {}. Restarting…",
            &local_before[..local_before.len().min(8)],
            &new_commit[..new_commit.len().min(8)]
        ));
        j["status"] = json!("restarting");
        Ok(())
    }
    .await;

    if let Err(e) = result {
        let mut j = state.job.lock().unwrap();
        j["status"] = json!("error");
        j["error"] = json!(e);
    }
    state.job.lock().unwrap()["finished_at"] = json!(Utc::now().to_rfc3339());

    if state.job_status() == "restarting" {
        // Give the HTTP response time to flush back to the caller before the
        // process exits — systemd's Restart=on-failure relaunches the binary
        // a few seconds later running the code (and, if rebuilt, the binary)
        // that was just pulled.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        std::process::exit(1);
    }
}
