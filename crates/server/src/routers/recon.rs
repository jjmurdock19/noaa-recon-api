//! Port of `app/routers/recon.py` — recon MET archive endpoints.

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::error::{ApiError, ApiResult};
use crate::services::recon_met;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    // NOTE: the /mission/* literal routes are registered here too; axum's router
    // resolves static segments ("years", "mission") ahead of the `:year` param,
    // so unlike Starlette we don't depend on registration order to avoid the
    // greedy `{year}` swallowing "/mission/<id>".
    Router::new()
        .route("/recon/years", get(list_years))
        .route("/recon/mission/:mission_id", get(get_mission))
        .route("/recon/mission/:mission_id/download", get(download_mission_source))
        .route("/recon/:year", get(list_storms_for_year))
        // `*storm_name` (catch-all) mirrors FastAPI's `{storm_name:path}`: the
        // "Training / Research" bucket contains a literal "/".
        .route("/recon/:year/*storm_name", get(list_missions_for_storm))
}

fn conn(state: &AppState) -> ApiResult<rusqlite::Connection> {
    Ok(recon_met::get_connection(&state.paths.recon_met_db)?)
}

async fn list_years(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let conn = conn(&state)?;
    Ok(Json(json!({ "years": recon_met::list_years(&conn)? })))
}

async fn get_mission(
    State(state): State<AppState>,
    Path(mission_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let conn = conn(&state)?;
    let mission = recon_met::get_mission(&conn, &mission_id)?
        .ok_or_else(|| ApiError::not_found(format!("Unknown mission_id: {mission_id}")))?;
    let obs = recon_met::get_observations(&conn, &mission_id)?;
    let obs_json: Vec<Value> = obs
        .iter()
        .map(|r| json!([r.unix_time, r.lat, r.lon, r.wind_kt, r.wind_dir, r.sfmr_kt, r.alt_m]))
        .collect();
    Ok(Json(json!({
        "mission_id": mission.mission_id,
        "year": mission.year,
        "storm_name": mission.storm_name,
        "storm_id": mission.storm_id,
        "aircraft": mission.aircraft,
        "tail_num": mission.tail_num,
        "flight_date": mission.flight_date,
        "start_unix": mission.start_unix,
        "end_unix": mission.end_unix,
        "source_url": mission.source_url,
        "obs_count": obs.len(),
        "obs": obs_json,
    })))
}

async fn list_storms_for_year(
    State(state): State<AppState>,
    Path(year): Path<i64>,
) -> ApiResult<Json<Value>> {
    let conn = conn(&state)?;
    let rows = recon_met::list_storms_for_year(&conn, year)?;
    if rows.is_empty() {
        return Err(ApiError::not_found(format!(
            "No recon missions found for year {year}."
        )));
    }
    let storms: Vec<Value> = rows
        .iter()
        .map(|r| json!({ "storm_name": r.storm_name, "storm_id": r.storm_id, "mission_count": r.mission_count }))
        .collect();
    Ok(Json(json!({ "year": year, "storms": storms })))
}

async fn list_missions_for_storm(
    State(state): State<AppState>,
    Path((year, storm_name)): Path<(i64, String)>,
) -> ApiResult<Json<Value>> {
    let conn = conn(&state)?;
    let rows = recon_met::list_missions_for_storm(&conn, year, &storm_name)?;
    if rows.is_empty() {
        return Err(ApiError::not_found(format!(
            "No recon missions found for '{storm_name}' in {year}."
        )));
    }
    let missions: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "mission_id": r.mission_id,
                "aircraft": r.aircraft,
                "tail_num": r.tail_num,
                "flight_date": r.flight_date,
                "start_unix": r.start_unix,
                "end_unix": r.end_unix,
                "obs_count": r.obs_count,
                "source_url": r.source_url,
            })
        })
        .collect();
    Ok(Json(json!({ "year": year, "storm_name": storm_name, "missions": missions })))
}

/// Streams NOAA's original full-resolution NetCDF straight through this API
/// (not a redirect) — port of `download_mission_source`.
async fn download_mission_source(
    State(state): State<AppState>,
    Path(mission_id): Path<String>,
) -> ApiResult<Response> {
    let source_url = {
        let conn = conn(&state)?;
        recon_met::get_mission(&conn, &mission_id)?
            .and_then(|m| m.source_url)
            .ok_or_else(|| {
                ApiError::not_found(format!("No source file on record for mission_id: {mission_id}"))
            })?
    };
    let filename = source_url.rsplit('/').next().unwrap_or("mission.nc").to_string();

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| ApiError::internal(format!("http client: {e}")))?;

    let upstream = client
        .get(&source_url)
        .send()
        .await
        .map_err(|e| ApiError::bad_gateway(format!("Failed to reach source file: {e}")))?;

    if upstream.status() != reqwest::StatusCode::OK {
        return Err(ApiError::bad_gateway(format!(
            "Source returned HTTP {} for {source_url}",
            upstream.status().as_u16()
        )));
    }

    let content_length = upstream
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let body = Body::from_stream(upstream.bytes_stream());
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/x-netcdf")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{filename}\""),
        );
    if let Some(len) = content_length {
        builder = builder.header(header::CONTENT_LENGTH, len);
    }
    builder
        .body(body)
        .map(|r| r.into_response())
        .map_err(|e| ApiError::internal(format!("response build: {e}")))
}
