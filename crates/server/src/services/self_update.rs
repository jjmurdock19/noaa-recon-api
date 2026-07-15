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
//! `cargo build --release -p noaa-recon-api` here. Unlike Python — which
//! re-interprets source on every restart, so a rebuild is only needed when
//! `pyproject.toml`'s dependencies changed — a Rust binary embeds its source
//! at compile time, so *any* pulled commit requires a rebuild, not just one
//! that touched `Cargo.lock`. This runs unconditionally after every pull.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use chrono::Utc;
use serde_json::{json, Value};
use tokio::process::Command;
use tokio::time::timeout;

const REMOTE: &str = "origin";

/// In-progress job statuses — mirrors Python's `_SELF_UPDATE_IN_PROGRESS_STATUSES`.
pub const IN_PROGRESS_STATUSES: &[&str] = &["checking", "pulling", "building"];

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

pub async fn current_branch(repo_root: &Path) -> Result<String, String> {
    git(repo_root, &["rev-parse", "--abbrev-ref", "HEAD"], 10).await
}

async fn working_tree_clean(repo_root: &Path) -> Result<bool, String> {
    Ok(git(repo_root, &["status", "--porcelain"], 15).await?.is_empty())
}

/// Every branch on `origin` (`git ls-remote --heads`) — backs the console's
/// branch-selector dropdown. Doesn't require a prior fetch; talks straight to
/// the remote, so it's always current.
pub async fn list_remote_branches(repo_root: &Path) -> Result<Vec<String>, String> {
    let out = git(repo_root, &["ls-remote", "--heads", REMOTE], 30).await?;
    let mut branches: Vec<String> = out
        .lines()
        .filter_map(|line| line.split('\t').nth(1).and_then(|r| r.strip_prefix("refs/heads/")))
        .map(str::to_string)
        .collect();
    branches.sort();
    Ok(branches)
}

/// Fetch the remote and report whether `branch` (default: whatever's
/// currently checked out) is behind it. Read-only — never pulls, checks out,
/// or otherwise modifies the working tree. Checking a branch other than the
/// checked-out one isn't a fast-forward comparison (there's no shared
/// "commits behind" — it's a different line of history entirely), so that
/// case reports `branch_switch: true` with the target's latest few commits
/// instead of a behind-count.
pub async fn check_for_update(repo_root: &Path, target_branch: Option<&str>) -> Result<Value, String> {
    let current = current_branch(repo_root).await?;
    let branch = target_branch.unwrap_or(&current).to_string();
    git(repo_root, &["fetch", REMOTE, &branch], 60).await?;
    let remote_ref = format!("{REMOTE}/{branch}");
    let remote = git(repo_root, &["rev-parse", &remote_ref], 10).await?;

    if branch != current {
        let log = git(repo_root, &["log", "--oneline", "-5", &remote_ref], 15).await?;
        let lines: Vec<&str> = if log.is_empty() { Vec::new() } else { log.lines().collect() };
        return Ok(json!({
            "branch": branch, "current_branch": current, "branch_switch": true,
            "up_to_date": false, "local_commit": Value::Null, "remote_commit": remote,
            "commits_behind": Value::Null, "log": lines,
        }));
    }

    let local = git(repo_root, &["rev-parse", "HEAD"], 10).await?;
    if local == remote {
        return Ok(json!({
            "branch": branch, "current_branch": current, "branch_switch": false,
            "up_to_date": true, "local_commit": local, "remote_commit": remote,
            "commits_behind": 0, "log": [],
        }));
    }
    let range = format!("HEAD..{remote_ref}");
    let log = git(repo_root, &["log", "--oneline", &range], 15).await?;
    let lines: Vec<&str> = if log.is_empty() { Vec::new() } else { log.lines().collect() };
    Ok(json!({
        "branch": branch, "current_branch": current, "branch_switch": false,
        "up_to_date": false, "local_commit": local, "remote_commit": remote,
        "commits_behind": lines.len(), "log": lines,
    }))
}

fn set_status(job: &Mutex<Value>, status: &str) {
    job.lock().unwrap()["status"] = json!(status);
}

/// Pull + rebuild (if `Cargo.lock` changed), then exit so systemd restarts
/// the process on the new binary. `target_branch`, if given and different
/// from whatever's currently checked out, switches branches first (`git
/// checkout` — same clean-working-tree requirement as the pull below, since
/// checkout can't safely proceed over uncommitted changes either). Mutates
/// the shared job state as it progresses so the console can poll status the
/// same way it already does for prefetch/archive-update jobs. Intended to
/// run as a detached background task — never awaited by the request handler
/// that starts it.
pub async fn apply_update(repo_root: PathBuf, state: std::sync::Arc<SelfUpdateState>, target_branch: Option<String>) {
    let result: Result<(), String> = async {
        set_status(&state.job, "checking");
        let current = current_branch(&repo_root).await?;
        let branch = target_branch.unwrap_or_else(|| current.clone());
        git(&repo_root, &["fetch", REMOTE, &branch], 60).await?;

        if branch != current {
            if !working_tree_clean(&repo_root).await? {
                return Err(
                    "Working tree has uncommitted changes on the server — refusing to switch \
                     branches. Resolve manually (git status) before retrying."
                        .to_string(),
                );
            }
            set_status(&state.job, "pulling");
            git(&repo_root, &["checkout", &branch], 30)
                .await
                .map_err(|e| format!("git checkout {branch} failed: {e}"))?;
        }

        let local_before = git(&repo_root, &["rev-parse", "HEAD"], 10).await?;
        let remote_ref = format!("{REMOTE}/{branch}");
        let remote = git(&repo_root, &["rev-parse", &remote_ref], 10).await?;
        if local_before == remote {
            let mut j = state.job.lock().unwrap();
            j["status"] = json!("up_to_date");
            j["result"] = json!(format!("Already up to date on {branch}."));
            j["branch"] = json!(branch);
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
        git(&repo_root, &["pull", "--ff-only", REMOTE, &branch], 60).await?;
        let new_commit = git(&repo_root, &["rev-parse", "HEAD"], 10).await?;

        // Always rebuild — the pulled commit may have changed .rs source with
        // no Cargo.lock diff at all, and a Rust binary won't pick that up
        // just by restarting (see the module doc comment).
        set_status(&state.job, "building");
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

        let mut j = state.job.lock().unwrap();
        j["new_commit"] = json!(new_commit);
        j["branch"] = json!(branch);
        j["result"] = json!(format!(
            "Updated {} -> {} on {branch}. Restarting…",
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
