//! Port of `app/routers/admin_tokens.py` — API token/account management, login
//! & usage logs, and the public-API auth toggle. All superuser-only except the
//! usage-log (any logged-in role).

use axum::extract::{Path, State};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use axum_extra::extract::cookie::SignedCookieJar;
use serde_json::{json, Value};

use crate::auth;
use crate::error::{ApiError, ApiResult};
use crate::services::tokens::{self, EditToken, NewToken};
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

async fn list_tokens(State(state): State<AppState>, jar: SignedCookieJar) -> ApiResult<Json<Value>> {
    auth::require_superuser(&jar)?;
    let conn = conn(&state)?;
    Ok(Json(json!({ "tokens": tokens::list_tokens(&conn)? })))
}

async fn create_token(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let session = auth::require_superuser(&jar)?;
    let role = body.get("role").and_then(Value::as_str).unwrap_or("");
    let owner_name = body.get("owner_name").and_then(Value::as_str).unwrap_or("").trim();
    if owner_name.is_empty() {
        return Err(ApiError::bad_request("owner_name is required"));
    }
    if !matches!(role, "superuser" | "moderator" | "regular") {
        return Err(ApiError::bad_request("role must be one of: superuser, moderator, regular"));
    }
    let username = body.get("username").and_then(Value::as_str);
    let password = body.get("password").and_then(Value::as_str);
    if matches!(role, "superuser" | "moderator") && !(username.is_some() && password.is_some()) {
        return Err(ApiError::bad_request(format!("{role} tokens require a username and password")));
    }

    let conn = conn(&state)?;
    let new = NewToken {
        role,
        owner_name,
        owner_email: body.get("owner_email").and_then(Value::as_str),
        notes: body.get("notes").and_then(Value::as_str),
        username,
        password,
        created_by: session.token_id,
    };
    match tokens::create_token(&conn, &new) {
        Ok((row, raw)) => {
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
    Path(token_id): Path<i64>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    auth::require_superuser(&jar)?;
    let conn = conn(&state)?;
    if tokens::get_token(&conn, token_id)?.is_none() {
        return Err(ApiError::not_found(format!("No token with id {token_id}")));
    }
    let edit = EditToken {
        owner_name: body.get("owner_name").and_then(Value::as_str),
        owner_email: body.get("owner_email").and_then(Value::as_str),
        notes: body.get("notes").and_then(Value::as_str),
        revoked: body.get("revoked").and_then(Value::as_bool),
        username: body.get("username").and_then(Value::as_str),
        password: body.get("password").and_then(Value::as_str),
    };
    let row = tokens::edit_token(&conn, token_id, &edit)?;
    Ok(Json(serde_json::to_value(row).unwrap()))
}

async fn delete_token(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Path(token_id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let session = auth::require_superuser(&jar)?;
    if session.token_id == Some(token_id) {
        return Err(ApiError::bad_request(
            "You can't delete the account you're currently logged in as",
        ));
    }
    let conn = conn(&state)?;
    if !tokens::delete_token(&conn, token_id)? {
        return Err(ApiError::not_found(format!("No token with id {token_id}")));
    }
    Ok(Json(json!({ "status": "deleted" })))
}

async fn regenerate_token(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Path(token_id): Path<i64>,
) -> ApiResult<Json<Value>> {
    auth::require_superuser(&jar)?;
    let conn = conn(&state)?;
    if tokens::get_token(&conn, token_id)?.is_none() {
        return Err(ApiError::not_found(format!("No token with id {token_id}")));
    }
    let (row, raw) = tokens::regenerate_token(&conn, token_id)?;
    let mut v = serde_json::to_value(&row).unwrap();
    v.as_object_mut().unwrap().insert("token".into(), json!(raw));
    Ok(Json(v))
}

#[derive(serde::Deserialize)]
struct LimitQuery {
    #[serde(default = "default_limit")]
    limit: i64,
    token_id: Option<i64>,
}
fn default_limit() -> i64 {
    200
}

async fn login_log(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    axum::extract::Query(q): axum::extract::Query<LimitQuery>,
) -> ApiResult<Json<Value>> {
    auth::require_superuser(&jar)?;
    let conn = conn(&state)?;
    Ok(Json(json!({ "entries": tokens::list_login_log(&conn, q.limit.clamp(1, 1000))? })))
}

async fn clear_login_log(State(state): State<AppState>, jar: SignedCookieJar) -> ApiResult<Json<Value>> {
    auth::require_superuser(&jar)?;
    let conn = conn(&state)?;
    Ok(Json(json!({ "cleared": tokens::clear_login_log(&conn)? })))
}

async fn usage_log(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    axum::extract::Query(q): axum::extract::Query<LimitQuery>,
) -> ApiResult<Json<Value>> {
    auth::require_login(&jar)?;
    let conn = conn(&state)?;
    Ok(Json(json!({
        "entries": tokens::list_usage_log(&conn, q.token_id, q.limit.clamp(1, 1000))?
    })))
}

async fn clear_usage_log(State(state): State<AppState>, jar: SignedCookieJar) -> ApiResult<Json<Value>> {
    auth::require_login(&jar)?;
    let conn = conn(&state)?;
    Ok(Json(json!({ "cleared": tokens::clear_usage_log(&conn)? })))
}

async fn get_auth_config(State(state): State<AppState>, jar: SignedCookieJar) -> ApiResult<Json<Value>> {
    auth::require_superuser(&jar)?;
    Ok(Json(json!({ "enabled": auth::is_auth_enabled(&state.paths.repo_root) })))
}

async fn set_auth_config(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    auth::require_superuser(&jar)?;
    let enabled = body
        .get("enabled")
        .and_then(Value::as_bool)
        .ok_or_else(|| ApiError::bad_request("body must include 'enabled': true|false"))?;
    auth::set_auth_enabled(&state.paths.repo_root, enabled)
        .map_err(|e| ApiError::internal(format!("write auth_config: {e}")))?;
    Ok(Json(json!({ "enabled": auth::is_auth_enabled(&state.paths.repo_root) })))
}
