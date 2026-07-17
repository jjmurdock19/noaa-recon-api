//! Tail Doppler Radar (TDR) endpoints. Discovery (`years`/`:year`/mission)
//! mirrors `recon.rs`'s shape now that `tdr_ingest.rs` builds a real index.
//! `sweep` reads/slices one indexed netCDF product (fetched + cached lazily
//! on first request — see `services/tdr_nc.rs`) into a Plotly-shaped grid.

use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use noaa_recon_core::sweep::colorscale_for_field;

use crate::error::{ApiError, ApiResult};
use crate::services::{tdr, tdr_nc};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    // Static segments ("years", "mission") resolve ahead of the `:year`
    // param in axum's router, same as recon.rs — registration order doesn't
    // matter for that, but grouping them together here mirrors it for
    // readability.
    Router::new()
        .route("/tdr/years", get(list_years))
        .route("/tdr/mission/:mission_id", get(get_mission))
        .route("/tdr/sweep", get(get_sweep))
        .route("/tdr/:year", get(list_storms_for_year))
        .route("/tdr/:year/*storm_name", get(list_missions_for_storm))
}

fn conn(state: &AppState) -> ApiResult<rusqlite::Connection> {
    Ok(tdr::get_connection(&state.paths.tdr_db)?)
}

async fn list_years(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let conn = conn(&state)?;
    Ok(Json(json!({ "years": tdr::list_years(&conn)? })))
}

async fn list_storms_for_year(
    State(state): State<AppState>,
    Path(year): Path<i64>,
) -> ApiResult<Json<Value>> {
    let conn = conn(&state)?;
    let rows = tdr::list_storms_for_year(&conn, year)?;
    if rows.is_empty() {
        return Err(ApiError::not_found(format!("No TDR missions found for year {year}.")));
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
    let rows = tdr::list_missions_for_storm(&conn, year, &storm_name)?;
    if rows.is_empty() {
        return Err(ApiError::not_found(format!("No TDR missions found for '{storm_name}' in {year}.")));
    }
    let missions: Vec<Value> = rows
        .iter()
        .map(|m| {
            json!({
                "mission_id": m.mission_id,
                "aircraft": m.aircraft,
                "tail_num": m.tail_num,
                "has_level1b": m.has_level1b,
                "has_level2": m.has_level2,
            })
        })
        .collect();
    Ok(Json(json!({ "year": year, "storm_name": storm_name, "missions": missions })))
}

async fn get_mission(
    State(state): State<AppState>,
    Path(mission_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let conn = conn(&state)?;
    let mission = tdr::get_mission(&conn, &mission_id)?
        .ok_or_else(|| ApiError::not_found(format!("Unknown TDR mission_id: {mission_id}")))?;
    let files = tdr::get_mission_files(&conn, &mission_id)?;
    let files_json: Vec<Value> = files
        .iter()
        .map(|f| {
            json!({
                "level": f.level,
                "product": f.product,
                "format": f.format,
                "analysis_time": f.analysis_time,
                "storm_relative": f.storm_relative,
                "fall_speed_removed": f.fall_speed_removed,
                "source_url": f.source_url,
            })
        })
        .collect();
    Ok(Json(json!({
        "mission_id": mission.mission_id,
        "year": mission.year,
        "aircraft": mission.aircraft,
        "tail_num": mission.tail_num,
        "storm_name": mission.storm_name,
        "storm_id": mission.storm_id,
        "has_level1b": mission.has_level1b,
        "has_level2": mission.has_level2,
        "file_count": files.len(),
        "files": files_json,
    })))
}

#[derive(Deserialize)]
struct SweepQuery {
    mission_id: String,
    /// Which source level's file to slice — `"1b"` or `"2"`. Defaults to
    /// `"2"` (QC'd) if that mission has a Level 2 file, else `"1b"`.
    level: Option<String>,
    /// One of `xy`, `xy_rel`, `vert_inbound`, `vert_inbound_rel`,
    /// `vert_inbound_fall`, `vert_outbound`, `vert_outbound_rel`,
    /// `vert_outbound_fall` — see `GET /v1/tdr/mission/{id}` for what a
    /// given mission actually has on file.
    product: String,
    /// `HHMM`, matching one of the mission's indexed analysis times.
    analysis_time: String,
    /// `xy`/`xy_rel`: reflectivity, radial_wind, tangential_wind, u, v, w,
    /// vort, wind_speed. `vert_*`: reflectivity, radial_wind,
    /// tangential_wind, wind_speed.
    field: String,
    /// `xy`/`xy_rel` only — CAPPI altitude in km, snapped to the nearest
    /// actual analysis level (returned as `z_km`). Ignored for `vert_*`
    /// products, which have no level axis. Defaults to 2.0km.
    z: Option<f32>,
}

async fn get_sweep(State(state): State<AppState>, Query(q): Query<SweepQuery>) -> ApiResult<Json<Value>> {
    let is_vert = q.product.starts_with("vert_");
    if !is_vert && !q.product.starts_with("xy") {
        return Err(ApiError::bad_request(format!(
            "Unknown product '{}' — expected xy, xy_rel, or a vert_inbound/vert_outbound variant.",
            q.product
        )));
    }

    let conn = tdr::get_connection(&state.paths.tdr_db)?;
    let mission = tdr::get_mission(&conn, &q.mission_id)?
        .ok_or_else(|| ApiError::not_found(format!("Unknown TDR mission_id: {}", q.mission_id)))?;
    let level = q.level.unwrap_or_else(|| if mission.has_level2 { "2".into() } else { "1b".into() });

    let file = tdr::find_file(&conn, &q.mission_id, &level, &q.product, &q.analysis_time, "nc")?.ok_or_else(|| {
        ApiError::not_found(format!(
            "No '{}' netCDF file on record for mission {} at level {level}, analysis_time {}. \
             Check GET /v1/tdr/mission/{} for what's actually indexed.",
            q.product, q.mission_id, q.analysis_time, q.mission_id
        ))
    })?;

    let cache_dir = state.paths.cache_root.join("tdr_nc");
    let cache_key = format!("{}_{level}_{}_{}", q.mission_id, q.product, q.analysis_time);
    let nc_path = tdr_nc::fetch_and_cache(&cache_dir, &file.source_url, &cache_key)
        .await
        .map_err(|e| ApiError::bad_gateway(format!("Failed to fetch/decompress source file: {e}")))?;

    let field = q.field.clone();
    let requested_z = q.z;
    let slice = tokio::task::spawn_blocking(move || {
        if is_vert {
            tdr_nc::read_vert_slice(&nc_path, &field)
        } else {
            tdr_nc::read_xy_slice(&nc_path, &field, requested_z)
        }
    })
    .await
    .map_err(|e| ApiError::internal(format!("slice task panicked: {e}")))?
    .map_err(|e| ApiError::bad_request(e.to_string()))?;

    let cs = colorscale_for_field(&q.field);
    let data: Vec<Vec<Option<f64>>> =
        slice.data.iter().map(|row| row.iter().map(|v| v.map(|x| x as f64)).collect()).collect();

    Ok(Json(json!({
        "mission_id": mission.mission_id,
        "storm_name": slice.storm_name_attr.unwrap_or(mission.storm_name),
        "level": level,
        "product": q.product,
        "analysis_time": q.analysis_time,
        "field": q.field,
        "z_km": slice.z_km,
        "x": slice.x,
        "y": slice.y,
        "data": data,
        "colorscale": cs.stops,
        "zmin": cs.zmin,
        "zmax": cs.zmax,
        "units": cs.units,
        "origin_lat": slice.origin_lat,
        "origin_lon": slice.origin_lon,
    })))
}
