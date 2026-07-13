//! Port of `app/routers/storms.py` — historical storm-track lookups.

use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::{ApiError, ApiResult};
use crate::services::storms;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/storms/years", get(list_years))
        .route("/storms/:year", get(list_storms_for_year))
        .route("/storms/:year/:name", get(get_storm_track))
        .route("/storms/:year/:name/nearest", get(get_nearest_point))
}

#[derive(Deserialize)]
struct BasinQuery {
    basin: Option<String>,
}

#[derive(Deserialize)]
struct NearestQuery {
    datetime: String,
    basin: Option<String>,
}

fn conn(state: &AppState) -> ApiResult<rusqlite::Connection> {
    Ok(storms::get_connection(&state.paths.storms_db)?)
}

/// Resolve exactly one storm or produce the 404/409 the Python router raises.
fn resolve_one_storm(
    conn: &rusqlite::Connection,
    year: i64,
    name: &str,
    basin: Option<&str>,
) -> ApiResult<storms::Storm> {
    let mut matches = storms::find_storms(conn, year, name, basin)?;
    if matches.is_empty() {
        return Err(ApiError::not_found(format!(
            "No storm named '{name}' found in {year}."
        )));
    }
    if matches.len() > 1 {
        let options = matches
            .iter()
            .map(|m| format!("{} ({} — {})", m.name, m.basin, m.atcf_id))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(ApiError::conflict(format!(
            "Ambiguous: multiple storms named '{name}' in {year} ({options}). Pass basin=AL|EP|CP to disambiguate."
        )));
    }
    Ok(matches.remove(0))
}

fn point_to_json(p: &storms::TrackPoint) -> Value {
    json!({
        "datetime_utc": p.datetime_utc,
        "status": p.status,
        "category": p.category,
        "lat": p.lat,
        "lon": p.lon,
        "wind_kt": p.wind_kt,
        "pressure_mb": p.pressure_mb,
    })
}

async fn list_years(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let conn = conn(&state)?;
    Ok(Json(json!({ "years": storms::list_years(&conn)? })))
}

async fn list_storms_for_year(
    State(state): State<AppState>,
    Path(year): Path<i64>,
) -> ApiResult<Json<Value>> {
    let conn = conn(&state)?;
    let rows = storms::list_storms_for_year(&conn, year)?;
    if rows.is_empty() {
        return Err(ApiError::not_found(format!("No storms found for year {year}.")));
    }
    let storms_json: Vec<Value> = rows
        .iter()
        .map(|r| json!({ "name": r.name, "basin": r.basin, "atcf_id": r.atcf_id }))
        .collect();
    Ok(Json(json!({ "year": year, "storms": storms_json })))
}

async fn get_storm_track(
    State(state): State<AppState>,
    Path((year, name)): Path<(i64, String)>,
    Query(q): Query<BasinQuery>,
) -> ApiResult<Json<Value>> {
    let conn = conn(&state)?;
    let storm = resolve_one_storm(&conn, year, &name, q.basin.as_deref())?;
    let points = storms::get_track(&conn, storm.id)?;
    Ok(Json(json!({
        "year": storm.year,
        "name": storm.name,
        "basin": storm.basin,
        "atcf_id": storm.atcf_id,
        "points": points.iter().map(point_to_json).collect::<Vec<_>>(),
    })))
}

async fn get_nearest_point(
    State(state): State<AppState>,
    Path((year, name)): Path<(i64, String)>,
    Query(q): Query<NearestQuery>,
) -> ApiResult<Json<Value>> {
    // Validate ISO-8601 (datetime.fromisoformat with Z -> +00:00).
    if chrono::DateTime::parse_from_rfc3339(&q.datetime).is_err() {
        return Err(ApiError::bad_request(format!(
            "datetime must be ISO 8601: {}",
            q.datetime
        )));
    }
    let conn = conn(&state)?;
    let storm = resolve_one_storm(&conn, year, &name, q.basin.as_deref())?;
    let point = storms::find_nearest_point(&conn, storm.id, &q.datetime)?
        .ok_or_else(|| ApiError::not_found("Storm has no track points on record."))?;
    let mut obj = point_to_json(&point);
    let map = obj.as_object_mut().unwrap();
    map.insert("year".into(), json!(storm.year));
    map.insert("name".into(), json!(storm.name));
    map.insert("basin".into(), json!(storm.basin));
    map.insert("atcf_id".into(), json!(storm.atcf_id));
    Ok(Json(obj))
}
