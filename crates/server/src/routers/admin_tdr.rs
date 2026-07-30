//! TDR mission admin: create / edit / delete — the write counterpart to the
//! public, read-only `routers/tdr.rs`. Viewing (years/storms/missions/files/
//! legs) stays on the existing public `/v1/tdr/*` routes, same as how the
//! console's Databases viewer already reuses `/v1/storms/*` and `/v1/recon/*`
//! read-only; this file only adds the mutations, gated by `tdr.manage`.
//!
//! Every mutation writes an audit entry naming the operator who made it, same
//! pattern as `admin_tokens.rs`.

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::routing::{delete, patch, post};
use axum::{Json, Router};
use axum_extra::extract::cookie::SignedCookieJar;
use serde_json::{json, Value};

use crate::auth;
use crate::error::{ApiError, ApiResult};
use crate::services::{tdr, tdr_ingest, tokens};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/tdr/missions", post(create_mission))
        .route("/admin/tdr/missions/:mission_id", patch(edit_mission).delete(delete_mission))
        .route("/admin/tdr/files/:file_id", delete(delete_file))
        .route("/admin/tdr/legs/:leg_id", delete(delete_leg))
}

fn conn(state: &AppState) -> ApiResult<rusqlite::Connection> {
    Ok(tdr::get_connection_rw(&state.paths.tdr_db)?)
}

fn audit(
    state: &AppState,
    user: &tokens::Token,
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

fn mission_json(m: &tdr::Mission) -> Value {
    json!({
        "mission_id": m.mission_id,
        "year": m.year,
        "aircraft": m.aircraft,
        "tail_num": m.tail_num,
        "storm_name": m.storm_name,
        "storm_id": m.storm_id,
        "storm_locked": m.storm_locked,
        "has_level1b": m.has_level1b,
        "has_level2": m.has_level2,
    })
}

async fn create_mission(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let actor = auth::require_permission(&state, &jar, "tdr.manage")?;

    let mission_id = body.get("mission_id").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if mission_id.is_empty() {
        return Err(ApiError::bad_request("mission_id is required"));
    }
    if !tdr_ingest::mission_id_re().is_match(&mission_id) {
        return Err(ApiError::bad_request(
            "mission_id must look like 'YYYYMMDDAI' — an 8-digit date, aircraft letter (H/I/N), \
             and flight sequence number, e.g. '20260616H1'",
        ));
    }
    let year = tdr_ingest::mission_year(&mission_id)
        .ok_or_else(|| ApiError::bad_request("could not determine a year from mission_id"))?;

    let conn = conn(&state)?;
    if tdr::get_mission(&conn, &mission_id)?.is_some() {
        return Err(ApiError::conflict(format!("Mission '{mission_id}' already exists")));
    }

    let new = tdr::NewMission {
        mission_id: &mission_id,
        year,
        aircraft: body.get("aircraft").and_then(Value::as_str),
        tail_num: body.get("tail_num").and_then(Value::as_str),
        storm_name: body.get("storm_name").and_then(Value::as_str),
        storm_id: body.get("storm_id").and_then(Value::as_str),
    };
    let row = tdr::create_mission(&conn, &new)?;
    audit(
        &state, &actor, "tdr.create",
        &format!("hand-created TDR mission '{mission_id}' ({})", row.storm_name),
        "/v1/admin/tdr/missions", "POST", &headers,
    );
    Ok(Json(mission_json(&row)))
}

async fn edit_mission(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    headers: HeaderMap,
    Path(mission_id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let actor = auth::require_permission(&state, &jar, "tdr.manage")?;
    let conn = conn(&state)?;
    let Some(before) = tdr::get_mission(&conn, &mission_id)? else {
        return Err(ApiError::not_found(format!("No TDR mission '{mission_id}'")));
    };

    // `storm_id` needs three states over the wire: key absent (leave alone),
    // present as "" (clear to NULL), present as text (set) — a plain
    // `and_then(Value::as_str)` can't tell "absent" from "present but empty".
    let storm_id_edit = body.get("storm_id").map(|v| {
        v.as_str().map(str::trim).filter(|s| !s.is_empty())
    });

    let edit = tdr::EditMission {
        aircraft: body.get("aircraft").and_then(Value::as_str),
        tail_num: body.get("tail_num").and_then(Value::as_str),
        storm_name: body.get("storm_name").and_then(Value::as_str),
        storm_id: storm_id_edit,
        has_level1b: body.get("has_level1b").and_then(Value::as_bool),
        has_level2: body.get("has_level2").and_then(Value::as_bool),
        storm_locked: body.get("storm_locked").and_then(Value::as_bool),
    };
    let row = tdr::edit_mission(&conn, &mission_id, &edit)?
        .ok_or_else(|| ApiError::not_found(format!("No TDR mission '{mission_id}'")))?;

    if before.storm_name != row.storm_name || before.storm_id != row.storm_id {
        audit(
            &state, &actor, "tdr.storm_identity",
            &format!(
                "corrected storm identity for '{mission_id}': '{}' ({}) -> '{}' ({})",
                before.storm_name, before.storm_id.as_deref().unwrap_or("—"),
                row.storm_name, row.storm_id.as_deref().unwrap_or("—"),
            ),
            "/v1/admin/tdr/missions/{mission_id}", "PATCH", &headers,
        );
    }
    if before.storm_locked != row.storm_locked {
        let verb = if row.storm_locked { "locked" } else { "unlocked" };
        audit(
            &state, &actor, "tdr.lock",
            &format!("{verb} storm identity for '{mission_id}' against future re-crawls"),
            "/v1/admin/tdr/missions/{mission_id}", "PATCH", &headers,
        );
    }
    let changed_fields: Vec<&str> = ["aircraft", "tail_num", "has_level1b", "has_level2"]
        .into_iter()
        .filter(|k| body.get(*k).is_some())
        .collect();
    if !changed_fields.is_empty() {
        audit(
            &state, &actor, "tdr.edit",
            &format!("updated {} on '{mission_id}'", changed_fields.join(", ")),
            "/v1/admin/tdr/missions/{mission_id}", "PATCH", &headers,
        );
    }
    Ok(Json(mission_json(&row)))
}

async fn delete_mission(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    headers: HeaderMap,
    Path(mission_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let actor = auth::require_permission(&state, &jar, "tdr.manage")?;
    let conn = conn(&state)?;
    let Some(before) = tdr::get_mission(&conn, &mission_id)? else {
        return Err(ApiError::not_found(format!("No TDR mission '{mission_id}'")));
    };
    tdr::delete_mission(&conn, &mission_id)?;
    audit(
        &state, &actor, "tdr.delete",
        &format!("deleted TDR mission '{mission_id}' ({}) — its files and legs went with it", before.storm_name),
        "/v1/admin/tdr/missions/{mission_id}", "DELETE", &headers,
    );
    Ok(Json(json!({ "status": "deleted" })))
}

async fn delete_file(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    headers: HeaderMap,
    Path(file_id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let actor = auth::require_permission(&state, &jar, "tdr.manage")?;
    let conn = conn(&state)?;
    if !tdr::delete_file(&conn, file_id)? {
        return Err(ApiError::not_found(format!("No TDR file record #{file_id}")));
    }
    audit(
        &state, &actor, "tdr.delete_file",
        &format!("deleted TDR file record #{file_id}"),
        "/v1/admin/tdr/files/{file_id}", "DELETE", &headers,
    );
    Ok(Json(json!({ "status": "deleted" })))
}

async fn delete_leg(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    headers: HeaderMap,
    Path(leg_id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let actor = auth::require_permission(&state, &jar, "tdr.manage")?;
    let conn = conn(&state)?;
    if !tdr::delete_leg(&conn, leg_id)? {
        return Err(ApiError::not_found(format!("No TDR leg record #{leg_id}")));
    }
    audit(
        &state, &actor, "tdr.delete_leg",
        &format!("deleted TDR leg record #{leg_id}"),
        "/v1/admin/tdr/legs/{leg_id}", "DELETE", &headers,
    );
    Ok(Json(json!({ "status": "deleted" })))
}
