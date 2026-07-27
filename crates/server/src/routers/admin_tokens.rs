//! Account / API-token management, login & usage logs, and the public-API auth
//! toggle.
//!
//! Access is per-permission, not per-role (see `services/tokens.rs`):
//! `users.view` to read the account list, `users.manage` to change it,
//! `loginlog.view` / `logs.clear` for the logs, `authconfig.manage` for the
//! public-API gate. The implicit superuser flag is the one thing `users.manage`
//! alone can't touch — granting it, or editing an account that holds it, needs
//! a superuser, so a delegated account manager can't promote itself.
//!
//! Every mutation here writes an audit entry naming the operator who made it.

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use axum_extra::extract::cookie::SignedCookieJar;
use serde_json::{json, Value};

use crate::auth;
use crate::error::{ApiError, ApiResult};
use crate::services::tokens::{self, EditToken, NewToken, Token};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/tokens", get(list_tokens).post(create_token))
        .route("/admin/tokens/:id", patch(edit_token).delete(delete_token))
        .route("/admin/tokens/:id/regenerate", post(regenerate_token))
        .route("/admin/login-log", get(login_log).delete(clear_login_log))
        .route("/admin/usage-log", get(usage_log).delete(clear_usage_log))
        .route("/admin/auth-config", get(get_auth_config).post(set_auth_config))
}

fn conn(state: &AppState) -> ApiResult<rusqlite::Connection> {
    Ok(tokens::get_connection(&state.paths.auth_db)?)
}

fn audit(
    state: &AppState,
    user: &Token,
    action: &str,
    detail: &str,
    endpoint: &str,
    method: &str,
    headers: &HeaderMap,
) {
    tokens::log_admin_action(
        &state.paths.auth_db,
        user,
        action,
        Some(detail),
        endpoint,
        method,
        auth::client_ip(headers).as_deref(),
    );
}

/// How an account is named in audit details and error messages.
fn label(t: &Token) -> String {
    match &t.username {
        Some(u) => format!("'{u}' (#{})", t.id),
        None => format!("API key '{}' (#{})", t.owner_name, t.id),
    }
}

/// Pull a `permissions: [...]` array out of a request body. `Ok(None)` means the
/// key was absent (leave grants alone); an empty array means "revoke everything".
fn read_permissions(body: &Value) -> ApiResult<Option<Vec<String>>> {
    let Some(raw) = body.get("permissions") else {
        return Ok(None);
    };
    let arr = raw
        .as_array()
        .ok_or_else(|| ApiError::bad_request("'permissions' must be an array of permission keys"))?;
    let mut out = Vec::with_capacity(arr.len());
    for v in arr {
        let key = v
            .as_str()
            .ok_or_else(|| ApiError::bad_request("'permissions' must contain strings"))?;
        if !tokens::is_known_permission(key) {
            return Err(ApiError::bad_request(format!("Unknown permission: '{key}'")));
        }
        out.push(key.to_string());
    }
    Ok(Some(out))
}

/// Human-readable summary of a permission change, for the audit detail.
fn describe_permission_change(before: &[String], after: &[String]) -> Option<String> {
    let granted: Vec<&str> = after.iter().filter(|p| !before.contains(p)).map(String::as_str).collect();
    let removed: Vec<&str> = before.iter().filter(|p| !after.contains(p)).map(String::as_str).collect();
    let mut parts = Vec::new();
    if !granted.is_empty() {
        parts.push(format!("granted {}", granted.join(", ")));
    }
    if !removed.is_empty() {
        parts.push(format!("removed {}", removed.join(", ")));
    }
    (!parts.is_empty()).then(|| parts.join("; "))
}

// ── Accounts ────────────────────────────────────────────────────────────────

async fn list_tokens(State(state): State<AppState>, jar: SignedCookieJar) -> ApiResult<Json<Value>> {
    auth::require_permission(&state, &jar, "users.view")?;
    let conn = conn(&state)?;
    Ok(Json(json!({ "tokens": tokens::list_tokens(&conn)? })))
}

async fn create_token(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let actor = auth::require_permission(&state, &jar, "users.manage")?;
    let owner_name = body.get("owner_name").and_then(Value::as_str).unwrap_or("").trim();
    if owner_name.is_empty() {
        return Err(ApiError::bad_request("owner_name is required"));
    }
    let username = body.get("username").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
    let password = body.get("password").and_then(Value::as_str).filter(|s| !s.is_empty());
    let is_superuser = body.get("is_superuser").and_then(Value::as_bool).unwrap_or(false);
    let permissions = read_permissions(&body)?.unwrap_or_default();

    if username.is_some() != password.is_some() {
        return Err(ApiError::bad_request(
            "A console account needs both a username and a password; leave both blank for an API-only key",
        ));
    }
    if username.is_none() && (is_superuser || !permissions.is_empty()) {
        return Err(ApiError::bad_request(
            "Console permissions require a username and password",
        ));
    }
    // Only a superuser can mint another superuser.
    if is_superuser && !actor.is_superuser {
        return Err(ApiError::new(
            axum::http::StatusCode::FORBIDDEN,
            "Only a superuser can create a superuser account",
        ));
    }

    let conn = conn(&state)?;
    let new = NewToken {
        owner_name,
        owner_email: body.get("owner_email").and_then(Value::as_str),
        notes: body.get("notes").and_then(Value::as_str),
        username,
        password,
        created_by: Some(actor.id),
        is_superuser,
        permissions,
    };
    match tokens::create_token(&conn, &new) {
        Ok((row, raw)) => {
            let kind = if row.is_superuser {
                "superuser account".to_string()
            } else if row.is_console_account() {
                format!("console account with {} permission(s)", row.permissions.len())
            } else {
                "API-only key".to_string()
            };
            audit(
                &state, &actor, "user.create",
                &format!("created {kind} {} for {}", label(&row), row.owner_name),
                "/v1/admin/tokens", "POST", &headers,
            );
            let mut v = serde_json::to_value(&row).unwrap();
            v.as_object_mut().unwrap().insert("token".into(), json!(raw));
            Ok(Json(v))
        }
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("UNIQUE constraint failed: tokens.username") {
                Err(ApiError::conflict(format!("username {username:?} is already taken")))
            } else {
                Err(ApiError::bad_request(msg))
            }
        }
    }
}

async fn edit_token(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    headers: HeaderMap,
    Path(token_id): Path<i64>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let actor = auth::require_permission(&state, &jar, "users.manage")?;
    let conn = conn(&state)?;
    let Some(before) = tokens::get_token(&conn, token_id)? else {
        return Err(ApiError::not_found(format!("No token with id {token_id}")));
    };

    let revoked = body.get("revoked").and_then(Value::as_bool);
    let is_superuser = body.get("is_superuser").and_then(Value::as_bool);
    let permissions = read_permissions(&body)?;

    // A delegated account manager may not touch superuser accounts or hand out
    // the flag — otherwise `users.manage` would be a path to full access.
    if (before.is_superuser || is_superuser.is_some()) && !actor.is_superuser {
        return Err(ApiError::new(
            axum::http::StatusCode::FORBIDDEN,
            "Only a superuser can modify a superuser account",
        ));
    }
    // Self-lockout guards: changing your own access is the one edit that can
    // leave nobody able to undo it.
    if actor.id == token_id {
        if permissions.is_some() {
            return Err(ApiError::bad_request(
                "You can't change your own permissions — ask another administrator",
            ));
        }
        if revoked == Some(true) {
            return Err(ApiError::bad_request("You can't revoke the account you're logged in as"));
        }
        if is_superuser == Some(false) {
            return Err(ApiError::bad_request("You can't remove your own superuser access"));
        }
    }
    // Never leave the system with no superuser at all.
    let losing_superuser = before.is_superuser && (is_superuser == Some(false) || revoked == Some(true));
    if losing_superuser && tokens::count_superusers(&conn)? <= 1 {
        return Err(ApiError::bad_request(
            "This is the only active superuser — grant superuser access to another account first",
        ));
    }

    let edit = EditToken {
        owner_name: body.get("owner_name").and_then(Value::as_str),
        owner_email: body.get("owner_email").and_then(Value::as_str),
        notes: body.get("notes").and_then(Value::as_str),
        revoked,
        username: body.get("username").and_then(Value::as_str),
        password: body.get("password").and_then(Value::as_str),
        is_superuser,
        permissions: permissions.clone(),
        actor_id: Some(actor.id),
    };
    let row = tokens::edit_token(&conn, token_id, &edit)?;

    // One audit entry per kind of change, so a permission grant is never hidden
    // inside a generic "edited account" line.
    if let (Some(after), Some(_)) = (&row, &permissions) {
        if let Some(change) = describe_permission_change(&before.permissions, &after.permissions) {
            audit(
                &state, &actor, "user.permissions",
                &format!("{change} for {}", label(after)),
                "/v1/admin/tokens/{id}", "PATCH", &headers,
            );
        }
    }
    if let Some(flag) = is_superuser {
        if flag != before.is_superuser {
            let verb = if flag { "granted" } else { "removed" };
            audit(
                &state, &actor, "user.superuser",
                &format!("{verb} superuser access for {}", label(&before)),
                "/v1/admin/tokens/{id}", "PATCH", &headers,
            );
        }
    }
    if let Some(flag) = revoked {
        if flag != before.revoked {
            let verb = if flag { "revoked" } else { "restored" };
            audit(
                &state, &actor, "user.revoke",
                &format!("{verb} {}", label(&before)),
                "/v1/admin/tokens/{id}", "PATCH", &headers,
            );
        }
    }
    let changed_fields: Vec<&str> = ["owner_name", "owner_email", "notes", "username", "password"]
        .into_iter()
        .filter(|k| body.get(*k).is_some())
        .collect();
    if !changed_fields.is_empty() {
        audit(
            &state, &actor, "user.edit",
            &format!("updated {} on {}", changed_fields.join(", "), label(&before)),
            "/v1/admin/tokens/{id}", "PATCH", &headers,
        );
    }
    Ok(Json(serde_json::to_value(row).unwrap()))
}

async fn delete_token(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    headers: HeaderMap,
    Path(token_id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let actor = auth::require_permission(&state, &jar, "users.manage")?;
    if actor.id == token_id {
        return Err(ApiError::bad_request(
            "You can't delete the account you're currently logged in as",
        ));
    }
    let conn = conn(&state)?;
    let Some(target) = tokens::get_token(&conn, token_id)? else {
        return Err(ApiError::not_found(format!("No token with id {token_id}")));
    };
    if target.is_superuser && !actor.is_superuser {
        return Err(ApiError::new(
            axum::http::StatusCode::FORBIDDEN,
            "Only a superuser can delete a superuser account",
        ));
    }
    if target.is_superuser && !target.revoked && tokens::count_superusers(&conn)? <= 1 {
        return Err(ApiError::bad_request(
            "This is the only active superuser — grant superuser access to another account first",
        ));
    }
    if !tokens::delete_token(&conn, token_id)? {
        return Err(ApiError::not_found(format!("No token with id {token_id}")));
    }
    audit(
        &state, &actor, "user.delete",
        &format!("deleted {} ({})", label(&target), target.owner_name),
        "/v1/admin/tokens/{id}", "DELETE", &headers,
    );
    Ok(Json(json!({ "status": "deleted" })))
}

async fn regenerate_token(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    headers: HeaderMap,
    Path(token_id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let actor = auth::require_permission(&state, &jar, "users.manage")?;
    let conn = conn(&state)?;
    let Some(target) = tokens::get_token(&conn, token_id)? else {
        return Err(ApiError::not_found(format!("No token with id {token_id}")));
    };
    if target.is_superuser && !actor.is_superuser {
        return Err(ApiError::new(
            axum::http::StatusCode::FORBIDDEN,
            "Only a superuser can regenerate a superuser account's token",
        ));
    }
    let (row, raw) = tokens::regenerate_token(&conn, token_id)?;
    audit(
        &state, &actor, "user.regenerate",
        &format!("issued a new API token for {}", label(&target)),
        "/v1/admin/tokens/{id}/regenerate", "POST", &headers,
    );
    let mut v = serde_json::to_value(&row).unwrap();
    v.as_object_mut().unwrap().insert("token".into(), json!(raw));
    Ok(Json(v))
}

// ── Logs ────────────────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct LimitQuery {
    #[serde(default = "default_limit")]
    limit: i64,
    token_id: Option<i64>,
    /// `admin` (console actions) or `api` (bearer-token calls); omitted = both.
    source: Option<String>,
}
fn default_limit() -> i64 {
    200
}

async fn login_log(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    axum::extract::Query(q): axum::extract::Query<LimitQuery>,
) -> ApiResult<Json<Value>> {
    auth::require_permission(&state, &jar, "loginlog.view")?;
    let conn = conn(&state)?;
    Ok(Json(json!({ "entries": tokens::list_login_log(&conn, q.limit.clamp(1, 1000))? })))
}

async fn clear_login_log(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let actor = auth::require_permission(&state, &jar, "logs.clear")?;
    let conn = conn(&state)?;
    let cleared = tokens::clear_login_log(&conn)?;
    audit(
        &state, &actor, "loginlog.clear",
        &format!("cleared the login log ({cleared} entries)"),
        "/v1/admin/login-log", "DELETE", &headers,
    );
    Ok(Json(json!({ "cleared": cleared })))
}

/// Readable by anyone who can reach the console — it's the activity feed the
/// dashboard shows, and it exposes no secrets. Clearing it is a separate,
/// scarcer permission, because it's also the audit trail.
async fn usage_log(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    axum::extract::Query(q): axum::extract::Query<LimitQuery>,
) -> ApiResult<Json<Value>> {
    auth::require_login(&state, &jar)?;
    let source = match q.source.as_deref() {
        None | Some("") | Some("all") => None,
        Some(s @ ("admin" | "api")) => Some(s),
        Some(other) => {
            return Err(ApiError::bad_request(format!(
                "source must be 'admin', 'api' or 'all' (got '{other}')"
            )))
        }
    };
    let conn = conn(&state)?;
    Ok(Json(json!({
        "entries": tokens::list_usage_log(&conn, q.token_id, source, q.limit.clamp(1, 1000))?
    })))
}

async fn clear_usage_log(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let actor = auth::require_permission(&state, &jar, "logs.clear")?;
    let conn = conn(&state)?;
    let cleared = tokens::clear_usage_log(&conn)?;
    // Recorded after the wipe, so the trail always shows who emptied it.
    audit(
        &state, &actor, "usagelog.clear",
        &format!("cleared the usage log ({cleared} entries)"),
        "/v1/admin/usage-log", "DELETE", &headers,
    );
    Ok(Json(json!({ "cleared": cleared })))
}

// ── Public-API auth toggle ──────────────────────────────────────────────────

async fn get_auth_config(State(state): State<AppState>, jar: SignedCookieJar) -> ApiResult<Json<Value>> {
    auth::require_permission(&state, &jar, "authconfig.manage")?;
    Ok(Json(json!({ "enabled": auth::is_auth_enabled(&state.paths.repo_root) })))
}

async fn set_auth_config(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let actor = auth::require_permission(&state, &jar, "authconfig.manage")?;
    let enabled = body
        .get("enabled")
        .and_then(Value::as_bool)
        .ok_or_else(|| ApiError::bad_request("body must include 'enabled': true|false"))?;
    auth::set_auth_enabled(&state.paths.repo_root, enabled)
        .map_err(|e| ApiError::internal(format!("write auth_config: {e}")))?;
    audit(
        &state, &actor, "authconfig.set",
        if enabled { "turned public API authentication ON" } else { "turned public API authentication OFF" },
        "/v1/admin/auth-config", "POST", &headers,
    );
    Ok(Json(json!({ "enabled": auth::is_auth_enabled(&state.paths.repo_root) })))
}
