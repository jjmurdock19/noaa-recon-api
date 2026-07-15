//! Port of `app/routers/admin.py` — console API: status, logs, cache browsing/
//! deletion. Login/session live here too. Archive-update and self-update jobs
//! run as detached background tasks polled by the console; bulk-prefetch still
//! depends on a piece not ported to Rust, so it returns 501 for now.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path as FsPath, PathBuf};
use std::time::SystemTime;

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use axum_extra::extract::cookie::SignedCookieJar;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::{self, Session};
use crate::error::{ApiError, ApiResult};
use crate::services::cache::ResultCache;
use crate::services::{goes, recon_met, storms, tokens};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/public-stats", get(public_stats))
        .route("/admin/login", post(login))
        .route("/admin/logout", post(logout))
        .route("/admin/whoami", get(whoami))
        .route("/admin/status", get(status))
        .route("/admin/logs", get(get_logs))
        .route("/admin/cache/satellite", get(list_satellite_cache).delete(clear_satellite_cache))
        .route("/admin/cache/satellite/:key", delete(delete_satellite_cache_entry))
        .route("/admin/cache/goes_nc", get(list_goes_nc_cache).delete(clear_goes_nc_cache))
        .route("/admin/cache/goes_nc/:filename", delete(delete_goes_nc_entry))
        .route("/admin/cache/goes_nc/:filename/info", get(get_goes_nc_info))
        .route("/admin/self-update/status", get(self_update_status))
        .route("/admin/self-update/branches", get(self_update_branches))
        .route("/admin/self-update/check", post(self_update_check))
        .route("/admin/self-update/apply", post(self_update_apply).get(self_update_job))
        .route("/admin/archive-update/:archive", post(start_archive_update).get(get_archive_update))
        // Bulk prefetch depends on a piece not ported to Rust yet:
        .route("/admin/prefetch", post(not_ported).get(not_ported))
}

fn sat_cache(state: &AppState) -> ApiResult<ResultCache> {
    ResultCache::new(state.paths.cache_root.join("satellite"), 600)
        .map_err(|e| ApiError::internal(format!("cache init: {e}")))
}

fn nc_dir(state: &AppState) -> PathBuf {
    state.paths.cache_root.join("goes_nc")
}

fn iso(t: SystemTime) -> Option<String> {
    Some(DateTime::<Utc>::from(t).to_rfc3339())
}

// ── Public status (no login) ─────────────────────────────────────────────────
async fn public_stats(State(state): State<AppState>) -> Json<Value> {
    Json(serde_json::to_value(state.stats.public()).unwrap())
}

// ── Auth ──────────────────────────────────────────────────────────────────────
async fn login(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> ApiResult<(SignedCookieJar, Json<Value>)> {
    let username = body.get("username").and_then(Value::as_str).unwrap_or("").to_string();
    let password = body.get("password").and_then(Value::as_str).unwrap_or("");
    let user_agent = headers.get("user-agent").and_then(|v| v.to_str().ok());

    let conn = tokens::get_connection(&state.paths.auth_db)?;
    let row = tokens::verify_admin_login(&conn, &username, password)?;
    tokens::record_login(&conn, &username, row.as_ref(), row.is_some(), None, user_agent)?;

    let row = row.ok_or_else(|| {
        ApiError::new(axum::http::StatusCode::UNAUTHORIZED, "Invalid username or password")
    })?;
    let session = Session {
        authenticated: true,
        role: Some(row.role.clone()),
        username: row.username.clone(),
        token_id: Some(row.id),
    };
    let jar = auth::write_session(jar, &session);
    Ok((
        jar,
        Json(json!({ "status": "ok", "role": row.role, "username": row.username, "token_id": row.id })),
    ))
}

async fn logout(jar: SignedCookieJar) -> (SignedCookieJar, Json<Value>) {
    (auth::clear_session(jar), Json(json!({ "status": "ok" })))
}

async fn whoami(jar: SignedCookieJar) -> Json<Value> {
    let s = auth::read_session(&jar);
    if !s.is_authenticated() {
        return Json(json!({ "authenticated": false }));
    }
    Json(json!({
        "authenticated": true,
        "role": s.role,
        "username": s.username,
        "token_id": s.token_id,
    }))
}

// ── Status / cache stats ──────────────────────────────────────────────────────
fn dir_stats(dir: &FsPath) -> (u64, u64) {
    let (mut count, mut bytes) = (0u64, 0u64);
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            if e.metadata().map(|m| m.is_file()).unwrap_or(false) {
                count += 1;
                bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    (count, bytes)
}

fn file_bytes(p: &FsPath) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

async fn status(State(state): State<AppState>, jar: SignedCookieJar) -> ApiResult<Json<Value>> {
    auth::require_login(&jar)?;
    let sat = sat_cache(&state)?;
    let sat_stats = sat.stats();
    let sat_bytes = sat_stats["bytes"].as_u64().unwrap_or(0);
    let (nc_count, nc_bytes) = dir_stats(&nc_dir(&state));
    let cache_total = sat_bytes + nc_bytes;

    let storms_conn = storms::get_connection(&state.paths.storms_db)?;
    let storm_count: i64 = storms_conn.query_row("SELECT COUNT(*) FROM storms", [], |r| r.get(0))?;
    let recon_conn = recon_met::get_connection(&state.paths.recon_met_db)?;
    let mission_count: i64 = recon_conn.query_row("SELECT COUNT(*) FROM missions", [], |r| r.get(0))?;

    let storms_bytes = file_bytes(&state.paths.storms_db);
    let recon_bytes = file_bytes(&state.paths.recon_met_db);
    let db_total = storms_bytes + recon_bytes;

    Ok(Json(json!({
        "healthy": true,
        "cache": {
            "satellite": sat_stats,
            "goes_nc": { "file_count": nc_count, "bytes": nc_bytes },
            "total_bytes": cache_total,
        },
        "databases": {
            "storms": { "bytes": storms_bytes, "storm_count": storm_count },
            "recon_met": { "bytes": recon_bytes, "mission_count": mission_count },
            "total_bytes": db_total,
        },
        "grand_total_bytes": cache_total + db_total,
    })))
}

// ── Log tail ──────────────────────────────────────────────────────────────────
#[derive(Deserialize)]
struct LogQuery {
    #[serde(default)]
    offset: u64,
    #[serde(default = "default_max_bytes")]
    max_bytes: u64,
}
fn default_max_bytes() -> u64 {
    65536
}

/// `logging::configure` uses `tracing_appender`'s daily rolling appender,
/// which names files `app.log.YYYY-MM-DD` (no plain `app.log` ever exists).
/// Pick the most recently modified `app.log*` file so today's rotation is
/// always the one tailed.
fn find_log_file(log_dir: &FsPath) -> Option<PathBuf> {
    let mut best: Option<(SystemTime, PathBuf)> = None;
    let entries = std::fs::read_dir(log_dir).ok()?;
    for e in entries.flatten() {
        let name = e.file_name();
        if !name.to_string_lossy().starts_with("app.log") {
            continue;
        }
        let Ok(meta) = e.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let mt = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        if best.as_ref().map(|(bt, _)| mt > *bt).unwrap_or(true) {
            best = Some((mt, e.path()));
        }
    }
    best.map(|(_, p)| p)
}

async fn get_logs(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Query(q): Query<LogQuery>,
) -> ApiResult<Json<Value>> {
    auth::require_login(&jar)?;
    let max_bytes = q.max_bytes.clamp(1024, 1_000_000);
    let Some(log_file) = find_log_file(&state.paths.repo_root.join("logs")) else {
        return Ok(Json(json!({ "text": "", "offset": 0, "reset": true })));
    };
    let size = file_bytes(&log_file);
    let reset = q.offset > size || (q.offset == 0 && size > max_bytes);
    let start = if reset { size.saturating_sub(max_bytes) } else { q.offset };

    let mut f = std::fs::File::open(&log_file).map_err(|e| ApiError::internal(e.to_string()))?;
    f.seek(SeekFrom::Start(start)).map_err(|e| ApiError::internal(e.to_string()))?;
    let mut buf = vec![0u8; max_bytes as usize];
    let n = f.read(&mut buf).map_err(|e| ApiError::internal(e.to_string()))?;
    buf.truncate(n);
    Ok(Json(json!({
        "text": String::from_utf8_lossy(&buf),
        "offset": start + n as u64,
        "reset": reset,
    })))
}

// ── Cache browsing / deletion ─────────────────────────────────────────────────
async fn list_satellite_cache(State(state): State<AppState>, jar: SignedCookieJar) -> ApiResult<Json<Value>> {
    auth::require_login(&jar)?;
    let cache = sat_cache(&state)?;
    let mut entries: Vec<Value> = Vec::new();
    for key in cache.list_keys() {
        let mut meta = cache.get_status(&key).unwrap_or_else(|| json!({ "status": "unknown" }));
        let obj = meta.as_object_mut().unwrap();
        let png = cache.output_path(&key, "png");
        let modified = std::fs::metadata(cache.output_path(&key, "json"))
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(iso);
        obj.insert("key".into(), json!(key));
        obj.insert("size_bytes".into(), json!(file_bytes(&png)));
        obj.insert("modified".into(), json!(modified));
        entries.push(meta);
    }
    entries.sort_by(|a, b| {
        b["modified"].as_str().unwrap_or("").cmp(a["modified"].as_str().unwrap_or(""))
    });
    Ok(Json(json!({ "entries": entries })))
}

async fn delete_satellite_cache_entry(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Path(key): Path<String>,
) -> ApiResult<Json<Value>> {
    auth::require_login(&jar)?;
    let freed = sat_cache(&state)?.delete(&key);
    Ok(Json(json!({ "status": "ok", "bytes_freed": freed })))
}

async fn clear_satellite_cache(State(state): State<AppState>, jar: SignedCookieJar) -> ApiResult<Json<Value>> {
    auth::require_login(&jar)?;
    let cache = sat_cache(&state)?;
    let freed: u64 = cache.list_keys().iter().map(|k| cache.delete(k)).sum();
    Ok(Json(json!({ "status": "ok", "bytes_freed": freed })))
}

async fn list_goes_nc_cache(State(state): State<AppState>, jar: SignedCookieJar) -> ApiResult<Json<Value>> {
    auth::require_login(&jar)?;
    let dir = nc_dir(&state);
    let mut files: Vec<(SystemTime, PathBuf)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            if e.metadata().map(|m| m.is_file()).unwrap_or(false) {
                // `.part` files are downloads still in flight — reported
                // separately below (with byte-level progress) instead of
                // showing up here as a mysteriously-growing "cached" file.
                if e.file_name().to_string_lossy().ends_with(".part") {
                    continue;
                }
                let mt = e.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
                files.push((mt, e.path()));
            }
        }
    }
    files.sort_by(|a, b| b.0.cmp(&a.0));
    let entries: Vec<Value> = files
        .iter()
        .map(|(mt, p)| {
            let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("").to_string();
            json!({
                "filename": name,
                "size_bytes": file_bytes(p),
                "modified": iso(*mt),
                "scan_start": p.file_name().and_then(|s| s.to_str()).and_then(goes::parse_scan_start).map(|d| d.to_rfc3339()),
            })
        })
        .collect();
    Ok(Json(json!({ "entries": entries, "in_progress": state.downloads.snapshot() })))
}

fn safe_nc_path(state: &AppState, filename: &str) -> ApiResult<PathBuf> {
    if filename.contains('/') || filename.contains("..") {
        return Err(ApiError::bad_request("invalid filename"));
    }
    let p = nc_dir(state).join(filename);
    if !p.exists() {
        return Err(ApiError::not_found("not found"));
    }
    Ok(p)
}

async fn get_goes_nc_info(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Path(filename): Path<String>,
) -> ApiResult<Json<Value>> {
    auth::require_login(&jar)?;
    let path = safe_nc_path(&state, &filename)?;
    let size = file_bytes(&path);
    let mut info = goes::nc_info(&path).map_err(|e| ApiError::internal(e.to_string()))?;
    let obj = info.as_object_mut().unwrap();
    obj.insert("filename".into(), json!(filename));
    obj.insert("size_bytes".into(), json!(size));
    Ok(Json(info))
}

async fn delete_goes_nc_entry(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Path(filename): Path<String>,
) -> ApiResult<Json<Value>> {
    auth::require_login(&jar)?;
    let path = safe_nc_path(&state, &filename)?;
    let freed = file_bytes(&path);
    std::fs::remove_file(&path).map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(Json(json!({ "status": "ok", "bytes_freed": freed })))
}

async fn clear_goes_nc_cache(State(state): State<AppState>, jar: SignedCookieJar) -> ApiResult<Json<Value>> {
    auth::require_login(&jar)?;
    let (_, before) = dir_stats(&nc_dir(&state));
    if let Ok(rd) = std::fs::read_dir(nc_dir(&state)) {
        for e in rd.flatten() {
            if e.metadata().map(|m| m.is_file()).unwrap_or(false) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    Ok(Json(json!({ "status": "ok", "bytes_freed": before })))
}

// ── Self-update (pull latest code from git, rebuild if needed, restart) ───────
// See services/self_update.rs for the git/restart mechanics. A background
// task (main.rs, started at startup) periodically refreshes the cached check
// below so the console can show an "update available" badge without the
// operator having to click anything; actually pulling + restarting only ever
// happens from the explicit apply endpoint.

#[derive(Deserialize)]
struct BranchQuery {
    branch: Option<String>,
}

async fn self_update_status(State(state): State<AppState>, jar: SignedCookieJar) -> ApiResult<Json<Value>> {
    auth::require_login(&jar)?;
    Ok(Json(json!({
        "check": state.self_update.get_cached_check(),
        "job": state.self_update.job.lock().unwrap().clone(),
    })))
}

/// Backs the console's branch-selector dropdown.
async fn self_update_branches(State(state): State<AppState>, jar: SignedCookieJar) -> ApiResult<Json<Value>> {
    auth::require_login(&jar)?;
    let branches = crate::services::self_update::list_remote_branches(&state.paths.repo_root)
        .await
        .map_err(|e| ApiError::bad_gateway(format!("Failed to list branches: {e}")))?;
    let current = crate::services::self_update::current_branch(&state.paths.repo_root)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(json!({ "branches": branches, "current": current })))
}

/// "Check now" button — bypasses the periodic timer for an immediate fetch.
/// `?branch=` checks a branch other than whatever's currently checked out
/// (see `check_for_update`'s `branch_switch` field); omitted, it checks the
/// current branch, same as the periodic background check.
async fn self_update_check(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Query(q): Query<BranchQuery>,
) -> ApiResult<Json<Value>> {
    auth::require_superuser(&jar)?;
    match crate::services::self_update::check_for_update(&state.paths.repo_root, q.branch.as_deref()).await {
        Ok(result) => {
            state.self_update.set_cached_check(Some(result), None);
            Ok(Json(state.self_update.get_cached_check()))
        }
        Err(e) => {
            state.self_update.set_cached_check(None, Some(e.clone()));
            Err(ApiError::bad_gateway(format!("Update check failed: {e}")))
        }
    }
}

async fn self_update_apply(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Query(q): Query<BranchQuery>,
) -> ApiResult<Json<Value>> {
    auth::require_superuser(&jar)?;
    {
        let mut job = state.self_update.job.lock().unwrap();
        if crate::services::self_update::IN_PROGRESS_STATUSES.contains(&job["status"].as_str().unwrap_or("")) {
            return Err(ApiError::conflict("An update is already in progress"));
        }
        *job = json!({
            "status": "checking",
            "started_at": Utc::now().to_rfc3339(),
            "finished_at": Value::Null, "result": Value::Null, "error": Value::Null, "new_commit": Value::Null,
            "branch": q.branch,
        });
    }
    let repo_root = state.paths.repo_root.clone();
    let su_state = state.self_update.clone();
    tokio::spawn(crate::services::self_update::apply_update(repo_root, su_state, q.branch));
    Ok(Json(state.self_update.job.lock().unwrap().clone()))
}

async fn self_update_job(State(state): State<AppState>, jar: SignedCookieJar) -> ApiResult<Json<Value>> {
    auth::require_login(&jar)?;
    Ok(Json(state.self_update.job.lock().unwrap().clone()))
}

// ── Archive update (storm-track / recon-MET ingest) ──────────────────────────
// See services/archive_update.rs — both ingests are incremental by construction
// (skip missions/seasons already up to date), so "force update" only means
// "run it now" rather than "rebuild the archive from scratch".

async fn start_archive_update(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Path(archive): Path<String>,
) -> ApiResult<Json<Value>> {
    auth::require_superuser(&jar)?;
    if state.archive_update.is_running(&archive) {
        return Err(ApiError::conflict("An update for this archive is already in progress"));
    }
    let job = crate::services::archive_update::start(&state.archive_update, &state.paths, &archive)
        .ok_or_else(|| ApiError::not_found(format!("Unknown archive: {archive}")))?;
    Ok(Json(job))
}

async fn get_archive_update(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Path(archive): Path<String>,
) -> ApiResult<Json<Value>> {
    auth::require_login(&jar)?;
    let job = state
        .archive_update
        .job(&archive)
        .ok_or_else(|| ApiError::not_found(format!("Unknown archive: {archive}")))?;
    Ok(Json(job))
}

// ── Not-yet-ported jobs ───────────────────────────────────────────────────────
async fn not_ported(jar: SignedCookieJar) -> ApiResult<Json<Value>> {
    auth::require_login(&jar)?;
    Err(ApiError::not_implemented(
        "This console job (bulk prefetch / archive-update) isn't ported to the Rust build yet. \
         Archive ingest is still run via the Python scripts.",
    ))
}
