//! Tail Doppler Radar (TDR) endpoints. Discovery (`years`/`:year`/mission)
//! mirrors `recon.rs`'s shape now that `tdr_ingest.rs` builds a real index.
//! `sweep` reads/slices one indexed netCDF product (fetched + cached lazily
//! on first request — see `services/tdr_nc.rs`) into a Plotly-shaped grid.

use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use noaa_recon_core::qc;
use noaa_recon_core::sweep;
use noaa_recon_core::sweep::colorscale_for_field;

use crate::error::{ApiError, ApiResult};
use crate::services::{tdr, tdr_nc};
use crate::state::AppState;

/// `qc=true` always means the real, indexed `product=xy` file at
/// `level=1b` — never a synthetic product string (see `noaa_recon_core::qc`'s
/// module doc and the dashboard's `resolveProductLevel`). No product-string
/// mangling anywhere: the DB lookup (`tdr::find_file`) still keys on the
/// literal `"xy"`/`"1b"` this validates.
fn check_qc_product(qc: bool, product: &str) -> ApiResult<()> {
    if qc && product != "xy" {
        return Err(ApiError::bad_request("Custom QC (qc=true) currently only supports product=xy"));
    }
    Ok(())
}

/// Shown to a client alongside `qc_applied`/`qc_summary` so every response
/// carries the caveat, not just the dashboard's own banner (see
/// `clients/tdr-dashboard/index.html`'s disclaimer copy, which this mirrors).
const QC_DISCLAIMER: &str = "Custom QC (Experimental): a locally-implemented, automated QC pass on top of \
NOAA/HRD's real-time Level 1b grid. Not an official NOAA/NHC/HRD product, and not reviewed or endorsed by \
them. Flags and masks statistical outliers in the finished grid, plus weak phantom reflectivity under \
strong winds, but cannot correct upstream radar or synthesis errors, and hasn't been validated against \
Level 2. Treat as a supplementary diagnostic.";

/// Inserts the QC fields into an already-built sweep/volume/composite
/// response object when a QC pass actually ran — including `qc_params`, the
/// exact tuning that produced it (defaults filled in), so a client can show
/// what's in effect without tracking defaults itself.
fn insert_qc_fields(response: &mut Value, report: Option<qc::QcReport>, params: Option<qc::QcParams>) {
    if let Some(report) = report {
        let obj = response.as_object_mut().expect("response is always a JSON object");
        obj.insert("qc_applied".into(), json!(true));
        obj.insert("qc_summary".into(), serde_json::to_value(report).unwrap());
        obj.insert("qc_params".into(), serde_json::to_value(params.unwrap_or_default()).unwrap());
        obj.insert("qc_disclaimer".into(), json!(QC_DISCLAIMER));
    }
}

/// One tunable Custom QC knob. The query parameter is `name`; the
/// [`qc::QcParams`] field it sets is `name` minus its `qc_` prefix. Drives
/// both [`QcTuning::resolve`]'s range checks and `GET /v1/tdr/qc/params`.
struct QcKnob {
    name: &'static str,
    /// `"float"`, `"int"`, or `"bool"` (bounds unused for bool).
    kind: &'static str,
    min: f32,
    max: f32,
    description: &'static str,
}

const QC_KNOBS: &[QcKnob] = &[
    QcKnob { name: "qc_mad_k", kind: "float", min: 0.5, max: 20.0,
        description: "Outlier threshold in MAD-sigma units for despike, vertical, azimuthal and cross-consistency checks. Lower = more aggressive." },
    QcKnob { name: "qc_window", kind: "int", min: 1.0, max: 10.0,
        description: "Despike/edge-trim neighborhood half-width in cells (2 = 5x5 window). Also the vertical-continuity window in levels." },
    QcKnob { name: "qc_min_neighbors", kind: "int", min: 1.0, max: 440.0,
        description: "Valid neighbors needed before a cell is despike/azimuthal/cross-consistency tested; also the genuine-echo count needed for the wind-clutter median." },
    QcKnob { name: "qc_min_coverage", kind: "int", min: 0.0, max: 440.0,
        description: "Cells with fewer valid neighbors than this are edge-trimmed regardless of value. Must be <= qc_min_neighbors. 0 disables edge trim." },
    QcKnob { name: "qc_ring_width_km", kind: "float", min: 0.5, max: 50.0,
        description: "Azimuthal-ring annulus width (km)." },
    QcKnob { name: "qc_clutter_enabled", kind: "bool", min: 0.0, max: 1.0,
        description: "Run the wind-clutter check (reflectivity only)." },
    QcKnob { name: "qc_clutter_min_wind_ms", kind: "float", min: 0.0, max: 100.0,
        description: "Wind speed (m/s) at or above which a weak echo counts as a phantom." },
    QcKnob { name: "qc_clutter_wind_frac_of_peak", kind: "float", min: 0.0, max: 1.0,
        description: "If > 0, the wind threshold drops to this fraction of the level's peak wind when that's lower than qc_clutter_min_wind_ms. 0 = off." },
    QcKnob { name: "qc_clutter_wind_search_km", kind: "float", min: 0.0, max: 50.0,
        description: "If > 0, a weak echo with no wind of its own uses the strongest wind within this many km. 0 = off (such cells are never flagged)." },
    QcKnob { name: "qc_clutter_max_height_km", kind: "float", min: 0.0, max: 20.0,
        description: "Only run the wind-clutter check on levels at or below this height (km). Omit for all levels." },
    QcKnob { name: "qc_clutter_cutoff_low_dbz", kind: "float", min: -20.0, max: 20.0,
        description: "dBZ cutoff for a weak storm (genuine-echo median <= qc_clutter_ref_low_dbz). Cells at or below the cutoff under strong wind are removed." },
    QcKnob { name: "qc_clutter_cutoff_high_dbz", kind: "float", min: -20.0, max: 20.0,
        description: "dBZ cutoff for a strong storm (median >= qc_clutter_ref_high_dbz). Also the floor a cell must exceed to count as a genuine echo in that median." },
    QcKnob { name: "qc_clutter_ref_low_dbz", kind: "float", min: -10.0, max: 60.0,
        description: "Genuine-echo median (dBZ) at or below which the low cutoff applies." },
    QcKnob { name: "qc_clutter_ref_high_dbz", kind: "float", min: -10.0, max: 60.0,
        description: "Genuine-echo median (dBZ) at or above which the high cutoff applies; linear in between." },
];

/// Optional Custom QC tuning, read from the same query string as the main
/// query struct (a second `Query` extractor — `#[serde(flatten)]` can't parse
/// numbers out of a query string). Every field overrides the matching
/// [`qc::QcParams`] default; see [`QC_KNOBS`] for bounds. Ignored unless
/// `qc=true`.
#[derive(Deserialize)]
struct QcTuning {
    qc_mad_k: Option<f32>,
    qc_window: Option<usize>,
    qc_min_neighbors: Option<usize>,
    qc_min_coverage: Option<usize>,
    qc_ring_width_km: Option<f32>,
    qc_clutter_enabled: Option<bool>,
    qc_clutter_min_wind_ms: Option<f32>,
    qc_clutter_wind_frac_of_peak: Option<f32>,
    qc_clutter_wind_search_km: Option<f32>,
    qc_clutter_max_height_km: Option<f32>,
    qc_clutter_cutoff_low_dbz: Option<f32>,
    qc_clutter_cutoff_high_dbz: Option<f32>,
    qc_clutter_ref_low_dbz: Option<f32>,
    qc_clutter_ref_high_dbz: Option<f32>,
}

/// `value`, if given, after checking it against `name`'s [`QC_KNOBS`] bounds.
fn bounded<T: Copy + Into<f64>>(name: &str, value: Option<T>) -> ApiResult<Option<T>> {
    let Some(v) = value else { return Ok(None) };
    let knob = QC_KNOBS.iter().find(|k| k.name == name).expect("every QcTuning field has a QC_KNOBS entry");
    let f: f64 = v.into();
    if !f.is_finite() || f < knob.min as f64 || f > knob.max as f64 {
        return Err(ApiError::bad_request(format!("{name}={f} is out of range [{}, {}]", knob.min, knob.max)));
    }
    Ok(Some(v))
}

impl QcTuning {
    /// The [`qc::QcParams`] to run with — `None` when `want_qc` is false (no
    /// QC, nothing validated). `400` on any out-of-range or inconsistent
    /// value rather than silently clamping it.
    fn resolve(&self, want_qc: bool) -> ApiResult<Option<qc::QcParams>> {
        if !want_qc {
            return Ok(None);
        }
        let usize_knob = |name: &str, v: Option<usize>| -> ApiResult<Option<usize>> {
            Ok(bounded(name, v.map(|n| n.min(u32::MAX as usize) as u32))?.map(|n| n as usize))
        };
        let d = qc::QcParams::default();
        let p = qc::QcParams {
            mad_k: bounded("qc_mad_k", self.qc_mad_k)?.unwrap_or(d.mad_k),
            window: usize_knob("qc_window", self.qc_window)?.unwrap_or(d.window),
            min_neighbors: usize_knob("qc_min_neighbors", self.qc_min_neighbors)?.unwrap_or(d.min_neighbors),
            min_coverage: usize_knob("qc_min_coverage", self.qc_min_coverage)?.unwrap_or(d.min_coverage),
            ring_width_km: bounded("qc_ring_width_km", self.qc_ring_width_km)?.unwrap_or(d.ring_width_km),
            clutter_enabled: self.qc_clutter_enabled.unwrap_or(d.clutter_enabled),
            clutter_min_wind_ms: bounded("qc_clutter_min_wind_ms", self.qc_clutter_min_wind_ms)?
                .unwrap_or(d.clutter_min_wind_ms),
            clutter_wind_frac_of_peak: bounded("qc_clutter_wind_frac_of_peak", self.qc_clutter_wind_frac_of_peak)?
                .unwrap_or(d.clutter_wind_frac_of_peak),
            clutter_wind_search_km: bounded("qc_clutter_wind_search_km", self.qc_clutter_wind_search_km)?
                .unwrap_or(d.clutter_wind_search_km),
            clutter_max_height_km: bounded("qc_clutter_max_height_km", self.qc_clutter_max_height_km)?
                .or(d.clutter_max_height_km),
            clutter_cutoff_low_dbz: bounded("qc_clutter_cutoff_low_dbz", self.qc_clutter_cutoff_low_dbz)?
                .unwrap_or(d.clutter_cutoff_low_dbz),
            clutter_cutoff_high_dbz: bounded("qc_clutter_cutoff_high_dbz", self.qc_clutter_cutoff_high_dbz)?
                .unwrap_or(d.clutter_cutoff_high_dbz),
            clutter_ref_low_dbz: bounded("qc_clutter_ref_low_dbz", self.qc_clutter_ref_low_dbz)?
                .unwrap_or(d.clutter_ref_low_dbz),
            clutter_ref_high_dbz: bounded("qc_clutter_ref_high_dbz", self.qc_clutter_ref_high_dbz)?
                .unwrap_or(d.clutter_ref_high_dbz),
        };
        if p.min_coverage > p.min_neighbors {
            return Err(ApiError::bad_request(format!(
                "qc_min_coverage ({}) must be <= qc_min_neighbors ({})",
                p.min_coverage, p.min_neighbors
            )));
        }
        if p.clutter_cutoff_low_dbz > p.clutter_cutoff_high_dbz {
            return Err(ApiError::bad_request(format!(
                "qc_clutter_cutoff_low_dbz ({}) must be <= qc_clutter_cutoff_high_dbz ({})",
                p.clutter_cutoff_low_dbz, p.clutter_cutoff_high_dbz
            )));
        }
        if p.clutter_ref_low_dbz > p.clutter_ref_high_dbz {
            return Err(ApiError::bad_request(format!(
                "qc_clutter_ref_low_dbz ({}) must be <= qc_clutter_ref_high_dbz ({})",
                p.clutter_ref_low_dbz, p.clutter_ref_high_dbz
            )));
        }
        Ok(Some(p))
    }
}

/// `GET /v1/tdr/qc/params` — every Custom QC tuning knob with its default,
/// bounds and description, for building tuning controls. Defaults come from
/// [`qc::QcParams::default`], so this never drifts from what a `qc=true`
/// request actually runs with.
async fn get_qc_params() -> Json<Value> {
    let defaults = serde_json::to_value(qc::QcParams::default()).unwrap();
    let knobs: Vec<Value> = QC_KNOBS
        .iter()
        .map(|k| {
            let field = k.name.trim_start_matches("qc_");
            let bounds = if k.kind == "bool" { json!(null) } else { json!({"min": k.min, "max": k.max}) };
            json!({
                "name": k.name,
                "type": k.kind,
                "default": defaults[field],
                "bounds": bounds,
                "description": k.description,
            })
        })
        .collect();
    Json(json!({ "params": knobs, "disclaimer": QC_DISCLAIMER }))
}

/// Fetches + decodes the paired `xy_rel` file for the D (cross-consistency)
/// check, once the caller has already looked it up (`tdr::find_file`,
/// synchronously, before any `.await` — a `&rusqlite::Connection` can never
/// be passed into an async fn that itself awaits, since `Connection: !Sync`
/// would make the whole handler's future non-`Send`; see `archive_update.rs`'s
/// module doc for the same constraint). `requested_z` is passed through
/// unchanged so the counterpart resolves to the *same* CAPPI level as
/// whatever the primary file resolved to.
async fn fetch_qc_counterpart_slice(
    cache_dir: &std::path::Path,
    counterpart_file: &tdr::FileRecord,
    mission_id: &str,
    level: &str,
    analysis_time: &str,
    field: &str,
    requested_z: Option<f32>,
) -> ApiResult<tdr_nc::FieldSlice> {
    let cache_key = format!("{mission_id}_{level}_xy_rel_{analysis_time}");
    let nc_path = tdr_nc::fetch_and_cache(cache_dir, &counterpart_file.source_url, &cache_key)
        .await
        .map_err(|e| ApiError::bad_gateway(format!("Failed to fetch/decompress source file: {e}")))?;
    let field = field.to_string();
    tokio::task::spawn_blocking(move || tdr_nc::read_xy_slice(&nc_path, &field, requested_z))
        .await
        .map_err(|e| ApiError::internal(format!("qc counterpart slice task panicked: {e}")))?
        .map_err(|e| ApiError::bad_request(e.to_string()))
}

/// Same as [`fetch_qc_counterpart_slice`] but for a whole `xy_rel` volume
/// (the D check against a [`tdr_nc::FieldVolume`]).
async fn fetch_qc_counterpart_volume(
    cache_dir: &std::path::Path,
    counterpart_file: &tdr::FileRecord,
    mission_id: &str,
    level: &str,
    analysis_time: &str,
    field: &str,
) -> ApiResult<tdr_nc::FieldVolume> {
    let cache_key = format!("{mission_id}_{level}_xy_rel_{analysis_time}");
    let nc_path = tdr_nc::fetch_and_cache(cache_dir, &counterpart_file.source_url, &cache_key)
        .await
        .map_err(|e| ApiError::bad_gateway(format!("Failed to fetch/decompress source file: {e}")))?;
    let field = field.to_string();
    tokio::task::spawn_blocking(move || tdr_nc::read_xy_volume(&nc_path, &field))
        .await
        .map_err(|e| ApiError::internal(format!("qc counterpart volume task panicked: {e}")))?
        .map_err(|e| ApiError::bad_request(e.to_string()))
}

pub fn router() -> Router<AppState> {
    // Static segments ("years", "mission") resolve ahead of the `:year`
    // param in axum's router, same as recon.rs — registration order doesn't
    // matter for that, but grouping them together here mirrors it for
    // readability.
    Router::new()
        .route("/tdr/years", get(list_years))
        .route("/tdr/mission/:mission_id", get(get_mission))
        .route("/tdr/sweep", get(get_sweep))
        .route("/tdr/volume", get(get_volume))
        .route("/tdr/composite", get(get_composite))
        .route("/tdr/composite/all", get(get_composite_all))
        .route("/tdr/plane_slice", get(get_plane_slice))
        .route("/tdr/centers", get(get_centers))
        .route("/tdr/qc/params", get(get_qc_params))
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
                "storm_name": m.storm_name,
                "storm_id": m.storm_id,
                "storm_locked": m.storm_locked,
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
                "id": f.id,
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
    // Real radar-on/radar-off times per leg, lifted from each leg's
    // `*_analysis.tar` bundle filename — see tdr_ingest.rs::parse_leg_filename.
    // Missions crawled before this existed will have none until re-crawled
    // (`ingest-tdr --force`); callers should fall back to analysis_times.
    let legs = tdr::get_mission_legs(&conn, &mission_id)?;
    let legs_json: Vec<Value> = legs
        .iter()
        .map(|l| {
            json!({
                "id": l.id,
                "level": l.level,
                "start_time": l.start_time,
                "stop_time": l.stop_time,
                "source_url": l.source_url,
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
        "storm_locked": mission.storm_locked,
        "has_level1b": mission.has_level1b,
        "has_level2": mission.has_level2,
        "file_count": files.len(),
        "files": files_json,
        "legs": legs_json,
        // Per-analysis jobfile metadata (composite acceptability, storm
        // center, motion) — see tdr_ingest.rs::parse_jobfile_analysis.
        "analyses": tdr::get_mission_analyses(&conn, &mission_id)?,
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
    /// Runs the experimental "Custom QC" pass (`noaa_recon_core::qc`) on the
    /// decoded grid before returning it — see the module doc comment there.
    /// Forces `level` to `"1b"` regardless of what was requested, and only
    /// `product=xy` is accepted. Adds `qc_applied`/`qc_summary`/
    /// `qc_disclaimer` to the response; omitted entirely when unset/false.
    qc: Option<bool>,
}

async fn get_sweep(
    State(state): State<AppState>,
    Query(q): Query<SweepQuery>,
    Query(tuning): Query<QcTuning>,
) -> ApiResult<Json<Value>> {
    let is_vert = q.product.starts_with("vert_");
    if !is_vert && !q.product.starts_with("xy") {
        return Err(ApiError::bad_request(format!(
            "Unknown product '{}' — expected xy, xy_rel, or a vert_inbound/vert_outbound variant.",
            q.product
        )));
    }
    let want_qc = q.qc.unwrap_or(false);
    let qc_params = tuning.resolve(want_qc)?;
    check_qc_product(want_qc, &q.product)?;

    let conn = conn(&state)?;
    let mission = tdr::get_mission(&conn, &q.mission_id)?
        .ok_or_else(|| ApiError::not_found(format!("Unknown TDR mission_id: {}", q.mission_id)))?;
    let level = if want_qc {
        "1b".to_string()
    } else {
        q.level.unwrap_or_else(|| if mission.has_level2 { "2".into() } else { "1b".into() })
    };

    let file = tdr::find_file(&conn, &q.mission_id, &level, &q.product, &q.analysis_time, "nc")?.ok_or_else(|| {
        ApiError::not_found(format!(
            "No '{}' netCDF file on record for mission {} at level {level}, analysis_time {}. \
             Check GET /v1/tdr/mission/{} for what's actually indexed.",
            q.product, q.mission_id, q.analysis_time, q.mission_id
        ))
    })?;
    // Looked up synchronously alongside the primary file, before any
    // `.await` — see `fetch_qc_counterpart_slice`'s doc comment.
    let counterpart_file =
        if want_qc { tdr::find_file(&conn, &q.mission_id, &level, "xy_rel", &q.analysis_time, "nc")? } else { None };

    let cache_dir = state.paths.cache_root.join("tdr_nc");
    let cache_key = format!("{}_{level}_{}_{}", q.mission_id, q.product, q.analysis_time);
    let nc_path = tdr_nc::fetch_and_cache(&cache_dir, &file.source_url, &cache_key)
        .await
        .map_err(|e| ApiError::bad_gateway(format!("Failed to fetch/decompress source file: {e}")))?;

    let counterpart = match &counterpart_file {
        Some(cf) => {
            Some(fetch_qc_counterpart_slice(&cache_dir, cf, &q.mission_id, &level, &q.analysis_time, &q.field, q.z).await?)
        }
        None => None,
    };

    let field = q.field.clone();
    let requested_z = q.z;
    let (mut slice, wind) = tokio::task::spawn_blocking(move || -> anyhow::Result<(tdr_nc::FieldSlice, Option<tdr_nc::FieldSlice>)> {
        if is_vert {
            Ok((tdr_nc::read_vert_slice(&nc_path, &field)?, None))
        } else {
            let slice = tdr_nc::read_xy_slice(&nc_path, &field, requested_z)?;
            Ok((slice, tdr_nc::read_qc_wind_slice(&nc_path, &field, requested_z, qc_params)?))
        }
    })
    .await
    .map_err(|e| ApiError::internal(format!("slice task panicked: {e}")))?
    .map_err(|e| ApiError::bad_request(e.to_string()))?;

    let qc_report = if want_qc {
        Some(tdr_nc::apply_qc_to_slice(&mut slice, &q.field, counterpart.as_ref(), wind.as_ref(), &qc_params.unwrap_or_default()))
    } else {
        None
    };

    let cs = colorscale_for_field(&q.field);
    let data: Vec<Vec<Option<f64>>> =
        slice.data.iter().map(|row| row.iter().map(|v| v.map(|x| x as f64)).collect()).collect();

    let mut response = json!({
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
    });
    insert_qc_fields(&mut response, qc_report, qc_params);
    Ok(Json(response))
}

#[derive(Deserialize)]
struct VolumeQuery {
    mission_id: String,
    /// `"1b"` or `"2"` — same default rule as `SweepQuery::level`.
    level: Option<String>,
    /// `xy` or `xy_rel` only — a vertical profile has no level axis to
    /// volume-render.
    product: String,
    analysis_time: String,
    field: String,
    /// Same meaning as `SweepQuery::qc` — forces `level` to `"1b"` and
    /// requires `product=xy`.
    qc: Option<bool>,
}

/// `qc`: same forcing behavior as `SweepQuery::qc` — see `check_qc_product`.
fn resolve_mission_and_file(
    conn: &rusqlite::Connection,
    mission_id: &str,
    level: &Option<String>,
    product: &str,
    analysis_time: &str,
    qc: bool,
) -> ApiResult<(tdr::Mission, tdr::FileRecord, String)> {
    if !product.starts_with("xy") {
        return Err(ApiError::bad_request(format!(
            "Unknown product '{product}' — expected xy or xy_rel (a vertical profile has no level axis)."
        )));
    }
    check_qc_product(qc, product)?;
    let mission = tdr::get_mission(conn, mission_id)?
        .ok_or_else(|| ApiError::not_found(format!("Unknown TDR mission_id: {mission_id}")))?;
    let level = if qc {
        "1b".to_string()
    } else {
        level.clone().unwrap_or_else(|| if mission.has_level2 { "2".into() } else { "1b".into() })
    };
    let file = tdr::find_file(conn, mission_id, &level, product, analysis_time, "nc")?.ok_or_else(|| {
        ApiError::not_found(format!(
            "No '{product}' netCDF file on record for mission {mission_id} at level {level}, \
             analysis_time {analysis_time}. Check GET /v1/tdr/mission/{mission_id} for what's actually indexed."
        ))
    })?;
    Ok((mission, file, level))
}

async fn get_volume(
    State(state): State<AppState>,
    Query(q): Query<VolumeQuery>,
    Query(tuning): Query<QcTuning>,
) -> ApiResult<Json<Value>> {
    let want_qc = q.qc.unwrap_or(false);
    let qc_params = tuning.resolve(want_qc)?;
    let conn = conn(&state)?;
    let (mission, file, level) =
        resolve_mission_and_file(&conn, &q.mission_id, &q.level, &q.product, &q.analysis_time, want_qc)?;
    let counterpart_file =
        if want_qc { tdr::find_file(&conn, &q.mission_id, &level, "xy_rel", &q.analysis_time, "nc")? } else { None };

    let cache_dir = state.paths.cache_root.join("tdr_nc");
    let cache_key = format!("{}_{level}_{}_{}", q.mission_id, q.product, q.analysis_time);
    let nc_path = tdr_nc::fetch_and_cache(&cache_dir, &file.source_url, &cache_key)
        .await
        .map_err(|e| ApiError::bad_gateway(format!("Failed to fetch/decompress source file: {e}")))?;

    let counterpart = match &counterpart_file {
        Some(cf) => Some(fetch_qc_counterpart_volume(&cache_dir, cf, &q.mission_id, &level, &q.analysis_time, &q.field).await?),
        None => None,
    };

    let field = q.field.clone();
    let (mut volume, wind) = tokio::task::spawn_blocking(move || {
        let volume = tdr_nc::read_xy_volume(&nc_path, &field)?;
        Ok::<_, anyhow::Error>((volume, tdr_nc::read_qc_wind_volume(&nc_path, &field, qc_params)?))
    })
    .await
    .map_err(|e| ApiError::internal(format!("volume read task panicked: {e}")))?
    .map_err(|e| ApiError::bad_request(e.to_string()))?;

    let qc_report = if want_qc {
        Some(tdr_nc::apply_qc_to_volume(&mut volume, &q.field, counterpart.as_ref(), wind.as_ref(), &qc_params.unwrap_or_default()))
    } else {
        None
    };

    let cs = colorscale_for_field(&q.field);
    let data: Vec<Vec<Vec<Option<f64>>>> = volume
        .data
        .iter()
        .map(|plane| plane.iter().map(|row| row.iter().map(|v| v.map(|x| x as f64)).collect()).collect())
        .collect();

    let mut response = json!({
        "mission_id": mission.mission_id,
        "storm_name": volume.storm_name_attr.unwrap_or(mission.storm_name),
        "level": level,
        "product": q.product,
        "analysis_time": q.analysis_time,
        "field": q.field,
        "x": volume.x,
        "y": volume.y,
        "levels_km": volume.levels,
        "data": data,
        "colorscale": cs.stops,
        "zmin": cs.zmin,
        "zmax": cs.zmax,
        "units": cs.units,
        "origin_lat": volume.origin_lat,
        "origin_lon": volume.origin_lon,
    });
    insert_qc_fields(&mut response, qc_report, qc_params);
    Ok(Json(response))
}

#[derive(Deserialize)]
struct CompositeQuery {
    mission_id: String,
    level: Option<String>,
    /// `xy` or `xy_rel` — either works for `mode=time` now that alignment is
    /// done by georeferenced offset rather than requiring identical grids;
    /// `xy_rel` is still the physically cleaner choice since its wind fields
    /// already have storm motion removed.
    product: String,
    field: String,
    /// `altitude`: max-value projection across every CAPPI level at one
    /// analysis time. `time`: storm-relative mosaic of one CAPPI level across
    /// every composite-acceptable analysis time in the mission.
    /// `time_volume`: same storm-center alignment as `time`, but mosaics
    /// *every* CAPPI level instead of collapsing to one — a genuine 3D
    /// composite, volume-shaped like `GET /v1/tdr/volume` rather than
    /// sweep-shaped.
    mode: String,
    /// Required for `mode=altitude`, ignored otherwise.
    analysis_time: Option<String>,
    /// `mode=time` only — which CAPPI level to mosaic. Defaults to 2.0km.
    /// Ignored for `mode=time_volume`, which mosaics every level.
    z: Option<f32>,
    /// `mode=time`/`time_volume` — the analysis time (HHMM) whose storm
    /// center every other analysis is aligned around, and which the
    /// response's `origin_lat`/`origin_lon` geolocate. Must be one of the
    /// analyses actually used. Defaults to the earliest one.
    reference_time: Option<String>,
    /// Same meaning as `SweepQuery::qc`, run per analysis-time file *before*
    /// mosaicking (never on the combined mosaic — a synthesis-stage artifact
    /// shouldn't get smeared across the composite). Scoped to checks A/B/C/E
    /// here — the D (cross-consistency) check is skipped for composites, to
    /// avoid multiplying the counterpart-file fetch by every analysis time
    /// in the mosaic.
    qc: Option<bool>,
}

/// `GET /v1/tdr/composite` — two ways to flatten a mission's TDR data into
/// one image, both reusing the sweep-response shape so the dashboard can
/// render either with the same heatmap code as `GET /v1/tdr/sweep`:
///
/// - `mode=altitude`: collapses one analysis time's whole level axis into a
///   single "composite reflectivity"-style plane (max value per x/y column).
/// - `mode=time`: builds one storm-relative composite out of one CAPPI
///   level across the mission's analysis times. Analyses whose jobfile marks
///   them **not acceptable for composite** are left out entirely
///   ([`select_composite_files`]). The rest are aligned on **one** storm
///   center — `reference_time`'s, default the earliest: each analysis's own
///   center comes from its jobfile (else its grid origin, see
///   [`analysis_center`]), and every output cell at some distance + radial
///   from the reference center is filled from the same distance + radial
///   about each analysis's own center
///   ([`noaa_recon_core::sweep::storm_centered_mosaic`]). So the storm's
///   core lines up across every analysis however far it moved between them,
///   rather than being smeared across the earth-relative track. Output
///   `x`/`y` are km east/north of the reference center. Where several
///   analyses cover a cell they're combined per
///   [`noaa_recon_core::sweep::combine_mode_for_field`] — maxed for
///   reflectivity (the standard composite-reflectivity convention),
///   averaged for everything else (wind/vorticity fields, where an extreme
///   from one analysis time shouldn't dominate the composite).
async fn get_composite(
    State(state): State<AppState>,
    Query(q): Query<CompositeQuery>,
    Query(tuning): Query<QcTuning>,
) -> ApiResult<Json<Value>> {
    build_composite(&state, &q, &tuning, false).await
}

/// `GET /v1/tdr/composite/all` — same query and response as
/// `GET /v1/tdr/composite`'s `mode=time`/`time_volume`, but **ignores** the
/// jobfiles' "not acceptable for composite" verdict: every analysis time in
/// the flight goes into the one mosaic. The analyses that would normally have
/// been dropped are listed in `detail.analysis_times_unsuitable_included`.
/// `mode=altitude` is rejected — it only ever uses one analysis time, so the
/// filter never applies to it.
async fn get_composite_all(
    State(state): State<AppState>,
    Query(q): Query<CompositeQuery>,
    Query(tuning): Query<QcTuning>,
) -> ApiResult<Json<Value>> {
    if q.mode == "altitude" {
        return Err(ApiError::bad_request(
            "mode=altitude uses a single analysis time, so the acceptability filter never applies — use /v1/tdr/composite."
                .to_string(),
        ));
    }
    build_composite(&state, &q, &tuning, true).await
}

/// Shared body of [`get_composite`] / [`get_composite_all`].
/// `include_unsuitable` keeps analyses whose jobfile marks them not
/// acceptable for composite (see [`select_composite_files`]).
async fn build_composite(
    state: &AppState,
    q: &CompositeQuery,
    tuning: &QcTuning,
    include_unsuitable: bool,
) -> ApiResult<Json<Value>> {
    let want_qc = q.qc.unwrap_or(false);
    let qc_params = tuning.resolve(want_qc)?;
    let conn = conn(state)?;
    let cache_dir = state.paths.cache_root.join("tdr_nc");

    if q.mode == "time_volume" {
        if !q.product.starts_with("xy") {
            return Err(ApiError::bad_request(format!(
                "Unknown product '{}' — expected xy or xy_rel.",
                q.product
            )));
        }
        check_qc_product(want_qc, &q.product)?;
        let mission = tdr::get_mission(&conn, &q.mission_id)?
            .ok_or_else(|| ApiError::not_found(format!("Unknown TDR mission_id: {}", q.mission_id)))?;
        let level =
            if want_qc { "1b".to_string() } else { q.level.clone().unwrap_or_else(|| if mission.has_level2 { "2".into() } else { "1b".into() }) };
        let files = tdr::find_files_for_product(&conn, &q.mission_id, &level, &q.product)?;
        if files.is_empty() {
            return Err(ApiError::not_found(format!(
                "No '{}' netCDF files on record for mission {} at level {level}.",
                q.product, q.mission_id
            )));
        }
        let analyses = tdr::get_mission_analyses(&conn, &q.mission_id)?;
        return get_composite_time_volume(mission, level, files, analyses, &cache_dir, q, qc_params, include_unsuitable).await;
    }

    let mut qc_report = qc::QcReport::default();
    let (mission, level, x, y, data, detail, origin) = match q.mode.as_str() {
        "altitude" => {
            let analysis_time = q.analysis_time.clone().ok_or_else(|| {
                ApiError::bad_request("mode=altitude requires analysis_time".to_string())
            })?;
            let (mission, file, level) =
                resolve_mission_and_file(&conn, &q.mission_id, &q.level, &q.product, &analysis_time, want_qc)?;
            let cache_key = format!("{}_{level}_{}_{}", q.mission_id, q.product, analysis_time);
            let nc_path = tdr_nc::fetch_and_cache(&cache_dir, &file.source_url, &cache_key)
                .await
                .map_err(|e| ApiError::bad_gateway(format!("Failed to fetch/decompress source file: {e}")))?;
            let field = q.field.clone();
            let (mut slice, wind) = tokio::task::spawn_blocking(move || {
                let slice = tdr_nc::read_xy_altitude_composite(&nc_path, &field)?;
                Ok::<_, anyhow::Error>((slice, tdr_nc::read_qc_wind_altitude_composite(&nc_path, &field, qc_params)?))
            })
            .await
            .map_err(|e| ApiError::internal(format!("composite task panicked: {e}")))?
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
            if want_qc {
                // No D (cross-consistency) check for composites — see
                // `CompositeQuery::qc`'s doc comment.
                qc_report = tdr_nc::apply_qc_to_slice(&mut slice, &q.field, None, wind.as_ref(), &qc_params.unwrap_or_default());
            }
            let origin = slice.origin_lat.zip(slice.origin_lon);
            (mission, level, slice.x, slice.y, slice.data, json!({"analysis_time": analysis_time}), origin)
        }
        "time" => {
            if !q.product.starts_with("xy") {
                return Err(ApiError::bad_request(format!(
                    "Unknown product '{}' — expected xy or xy_rel.",
                    q.product
                )));
            }
            check_qc_product(want_qc, &q.product)?;
            let mission = tdr::get_mission(&conn, &q.mission_id)?
                .ok_or_else(|| ApiError::not_found(format!("Unknown TDR mission_id: {}", q.mission_id)))?;
            let level = if want_qc {
                "1b".to_string()
            } else {
                q.level.clone().unwrap_or_else(|| if mission.has_level2 { "2".into() } else { "1b".into() })
            };
            let files = tdr::find_files_for_product(&conn, &q.mission_id, &level, &q.product)?;
            if files.is_empty() {
                return Err(ApiError::not_found(format!(
                    "No '{}' netCDF files on record for mission {} at level {level}.",
                    q.product, q.mission_id
                )));
            }
            let analyses = tdr::get_mission_analyses(&conn, &q.mission_id)?;
            let (included, excluded, unsuitable) = select_composite_files(files, &analyses, &level, include_unsuitable)?;
            let requested_z = q.z.unwrap_or(2.0);

            // Read every included analysis time's slice first (need them all
            // in hand before centering). Custom QC runs per slice, here,
            // before mosaicking — never on the combined mosaic, so a
            // synthesis-stage artifact in one analysis time can't get smeared
            // across the composite.
            let mut slices = Vec::with_capacity(included.len());
            for (file, analysis) in &included {
                let cache_key = format!("{}_{level}_{}_{}", q.mission_id, q.product, file.analysis_time);
                let nc_path = tdr_nc::fetch_and_cache(&cache_dir, &file.source_url, &cache_key)
                    .await
                    .map_err(|e| ApiError::bad_gateway(format!("Failed to fetch/decompress source file: {e}")))?;
                let field = q.field.clone();
                let (mut slice, wind) = tokio::task::spawn_blocking(move || {
                    let slice = tdr_nc::read_xy_slice(&nc_path, &field, Some(requested_z))?;
                    Ok::<_, anyhow::Error>((slice, tdr_nc::read_qc_wind_slice(&nc_path, &field, Some(requested_z), qc_params)?))
                })
                .await
                .map_err(|e| ApiError::internal(format!("composite task panicked: {e}")))?
                .map_err(|e| ApiError::bad_request(e.to_string()))?;
                if want_qc {
                    let r = tdr_nc::apply_qc_to_slice(&mut slice, &q.field, None, wind.as_ref(), &qc_params.unwrap_or_default());
                    qc_report.merge(r);
                }
                let center = analysis_center(slice.origin_lat, slice.origin_lon, analysis.as_ref());
                slices.push((file.analysis_time.clone(), slice, center));
            }

            let times: Vec<String> = slices.iter().map(|(t, _, _)| t.clone()).collect();
            let centers: Vec<&AnalysisCenter> = slices.iter().map(|(_, _, c)| c).collect();
            let reference = reference_index(&times, q.reference_time.as_deref())?;
            let planes: Vec<sweep::StormPlane> = slices
                .iter()
                .map(|(_, s, c)| sweep::StormPlane {
                    x: &s.x,
                    y: &s.y,
                    data: &s.data,
                    center_x_km: c.x_km,
                    center_y_km: c.y_km,
                })
                .collect();
            let combine_mode = sweep::combine_mode_for_field(&q.field);
            let mosaic = sweep::storm_centered_mosaic(&planes, reference, combine_mode);

            let mut detail = centering_detail(&times, &centers, reference, &excluded, &unsuitable, combine_mode);
            detail["z_km"] = json!(requested_z);
            let origin = centers[reference].lat.zip(centers[reference].lon);
            (mission, level, mosaic.x, mosaic.y, mosaic.data, detail, origin)
        }
        other => {
            return Err(ApiError::bad_request(format!(
                "Unknown mode '{other}' — expected 'altitude', 'time', or 'time_volume'."
            )));
        }
    };

    let cs = colorscale_for_field(&q.field);
    let data_out: Vec<Vec<Option<f64>>> = data.iter().map(|row| row.iter().map(|v| v.map(|x| x as f64)).collect()).collect();

    let mut response = json!({
        "mission_id": mission.mission_id,
        "storm_name": mission.storm_name,
        "level": level,
        "product": q.product,
        "field": q.field,
        "mode": q.mode,
        "detail": detail,
        "x": x,
        "y": y,
        "data": data_out,
        "origin_lat": origin.map(|(la, _)| la),
        "origin_lon": origin.map(|(_, lo)| lo),
        "colorscale": cs.stops,
        "zmin": cs.zmin,
        "zmax": cs.zmax,
        "units": cs.units,
    });
    insert_qc_fields(&mut response, want_qc.then_some(qc_report), qc_params);
    Ok(Json(response))
}

/// Splits a mission's files for a `mode=time`/`time_volume` composite into
/// the analysis times it may use and the ones it must leave out: any
/// analysis whose jobfile marks it **not acceptable for composite** (HRD's
/// own `<acceptable>0</acceptable>` — typically a grid that wasn't centered
/// on the storm) is excluded; one with no verdict on record is kept. Each
/// kept file comes back paired with its jobfile metadata (see
/// [`tdr::analysis_for`]) for centering. Errors when nothing's left.
///
/// With `include_unsuitable` (`GET /v1/tdr/composite/all`) nothing is
/// excluded; the flagged analysis times are kept and returned in the third
/// list instead, so the response can say which ones overrode the verdict.
#[allow(clippy::type_complexity)]
fn select_composite_files(
    files: Vec<tdr::FileRecord>,
    analyses: &[tdr::AnalysisRecord],
    level: &str,
    include_unsuitable: bool,
) -> ApiResult<(Vec<(tdr::FileRecord, Option<tdr::AnalysisRecord>)>, Vec<Value>, Vec<String>)> {
    let total = files.len();
    let mut included = Vec::new();
    let mut excluded = Vec::new();
    let mut unsuitable = Vec::new();
    for file in files {
        let analysis = tdr::analysis_for(analyses, level, &file.analysis_time).cloned();
        if analysis.as_ref().and_then(|a| a.acceptable_for_composite) == Some(false) {
            if include_unsuitable {
                unsuitable.push(file.analysis_time.clone());
                included.push((file, analysis));
                continue;
            }
            excluded.push(json!({
                "analysis_time": file.analysis_time,
                "reason": "jobfile marks this analysis not acceptable for composite",
            }));
            continue;
        }
        included.push((file, analysis));
    }
    if included.is_empty() {
        return Err(ApiError::bad_request(format!(
            "All {total} analysis time(s) are marked not acceptable for composite in their jobfiles — nothing to composite."
        )));
    }
    Ok((included, excluded, unsuitable))
}

/// One analysis's storm center: where it is on the earth (when known), and
/// where it sits inside that analysis's own grid.
struct AnalysisCenter {
    lat: Option<f32>,
    lon: Option<f32>,
    x_km: f32,
    y_km: f32,
    /// `"jobfile"` or `"grid_origin"`.
    source: &'static str,
}

/// An analysis's storm center — its jobfile center when on record, placed
/// inside the grid by its distance + radial from the grid origin
/// ([`sweep::center_in_grid_km`]); otherwise the grid origin itself
/// (`ORIGIN_LATITUDE/LONGITUDE`), which the TDR synthesis builds each grid
/// around. With no origin attribute either, the grid's own `(0, 0)` still is
/// the analysis center by construction — just not geolocated.
fn analysis_center(origin_lat: Option<f32>, origin_lon: Option<f32>, analysis: Option<&tdr::AnalysisRecord>) -> AnalysisCenter {
    let jobfile = analysis.and_then(|a| Some((a.center_lat? as f32, a.center_lon? as f32)));
    match (jobfile, origin_lat.zip(origin_lon)) {
        (Some((lat, lon)), Some((olat, olon))) => {
            let (x_km, y_km) = sweep::center_in_grid_km(olat, olon, lat, lon);
            AnalysisCenter { lat: Some(lat), lon: Some(lon), x_km, y_km, source: "jobfile" }
        }
        (Some((lat, lon)), None) => AnalysisCenter { lat: Some(lat), lon: Some(lon), x_km: 0.0, y_km: 0.0, source: "jobfile" },
        (None, origin) => AnalysisCenter {
            lat: origin.map(|o| o.0),
            lon: origin.map(|o| o.1),
            x_km: 0.0,
            y_km: 0.0,
            source: "grid_origin",
        },
    }
}

/// The composite's one reference center: `reference_time` if the caller
/// asked for one, otherwise the earliest analysis time used.
fn reference_index(times: &[String], requested: Option<&str>) -> ApiResult<usize> {
    match requested {
        Some(t) => times.iter().position(|x| x == t).ok_or_else(|| {
            ApiError::bad_request(format!(
                "reference_time '{t}' isn't one of the analysis times in this composite: {times:?}"
            ))
        }),
        None => Ok(0),
    }
}

/// The `detail` object shared by `mode=time` and `mode=time_volume`: which
/// analyses were used/excluded, the reference center everything was aligned
/// on, and each analysis's own center with its distance + radial from the
/// reference (i.e. how far the storm moved relative to it).
fn centering_detail(
    times: &[String],
    centers: &[&AnalysisCenter],
    reference: usize,
    excluded: &[Value],
    unsuitable_included: &[String],
    combine_mode: sweep::CombineMode,
) -> Value {
    let r = centers[reference];
    let per_analysis: Vec<Value> = times
        .iter()
        .zip(centers)
        .map(|(t, c)| {
            let from_reference = match (r.lat, r.lon, c.lat, c.lon) {
                (Some(rlat), Some(rlon), Some(lat), Some(lon)) => {
                    let (distance_km, bearing_deg) = sweep::distance_bearing_km(rlat, rlon, lat, lon);
                    json!({"distance_km": distance_km, "bearing_deg": bearing_deg})
                }
                _ => Value::Null,
            };
            json!({
                "analysis_time": t,
                "lat": c.lat,
                "lon": c.lon,
                "source": c.source,
                "center_in_grid_km": {"x": c.x_km, "y": c.y_km},
                "from_reference": from_reference,
            })
        })
        .collect();
    json!({
        "centering": "storm-relative: every analysis re-plotted by distance + radial from its own storm center around one shared reference center",
        "analysis_times_used": times,
        "analysis_times_excluded": excluded,
        "analysis_times_unsuitable_included": unsuitable_included,
        "reference_analysis_time": times[reference],
        "reference_center": {"lat": r.lat, "lon": r.lon, "source": r.source},
        "reference_origin": {"lat": r.lat, "lon": r.lon},
        "analysis_centers": per_analysis,
        "combine_mode": if combine_mode == sweep::CombineMode::Max { "max" } else { "mean" },
    })
}

/// `mode=time_volume` — the 3D counterpart to `mode=time`: instead of
/// collapsing to one CAPPI level before mosaicking, this reads every
/// analysis time's *entire* volume and mosaics level-by-level (same
/// acceptability filter and storm-relative centering as `mode=time`, run
/// once per level), so the result is a genuine 3D composite the dashboard can
/// feed straight into the same volumetric raymarch renderer as
/// `GET /v1/tdr/volume` — hence the volume-shaped (not sweep-shaped) response.
// Takes mission/level/files already resolved (owned, not a `&Connection` —
// rusqlite's Connection isn't Sync, so a reference to it can't cross the
// `.await`s below without making the whole handler's future non-Send; the
// caller does the DB lookups synchronously and hands off owned data).
#[allow(clippy::too_many_arguments)]
async fn get_composite_time_volume(
    mission: tdr::Mission,
    level: String,
    files: Vec<tdr::FileRecord>,
    analyses: Vec<tdr::AnalysisRecord>,
    cache_dir: &std::path::Path,
    q: &CompositeQuery,
    qc_params: Option<qc::QcParams>,
    include_unsuitable: bool,
) -> ApiResult<Json<Value>> {
    let want_qc = qc_params.is_some();
    let (included, mut excluded, unsuitable) = select_composite_files(files, &analyses, &level, include_unsuitable)?;

    // Read every included analysis time's whole volume first — need them all
    // in hand before centering and picking the canonical level grid. Custom
    // QC (no D/cross-consistency check — see `CompositeQuery::qc`) runs on
    // each volume here, before mosaicking.
    let mut qc_report = qc::QcReport::default();
    let mut volumes = Vec::with_capacity(included.len());
    for (file, analysis) in &included {
        let cache_key = format!("{}_{level}_{}_{}", q.mission_id, q.product, file.analysis_time);
        let nc_path = tdr_nc::fetch_and_cache(cache_dir, &file.source_url, &cache_key)
            .await
            .map_err(|e| ApiError::bad_gateway(format!("Failed to fetch/decompress source file: {e}")))?;
        let field = q.field.clone();
        let (mut volume, wind) = tokio::task::spawn_blocking(move || {
            let volume = tdr_nc::read_xy_volume(&nc_path, &field)?;
            Ok::<_, anyhow::Error>((volume, tdr_nc::read_qc_wind_volume(&nc_path, &field, qc_params)?))
        })
        .await
        .map_err(|e| ApiError::internal(format!("composite task panicked: {e}")))?
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
        if want_qc {
            let r = tdr_nc::apply_qc_to_volume(&mut volume, &q.field, None, wind.as_ref(), &qc_params.unwrap_or_default());
            qc_report.merge(r);
        }
        let center = analysis_center(volume.origin_lat, volume.origin_lon, analysis.as_ref());
        volumes.push((file.analysis_time.clone(), volume, center));
    }

    // Every volume needs the reference analysis's level grid to mosaic
    // level-by-level — drop (and report) any whose levels don't match rather
    // than guessing how to reconcile mismatched CAPPI grids.
    let all_times: Vec<String> = volumes.iter().map(|(t, _, _)| t.clone()).collect();
    let reference_levels = volumes[reference_index(&all_times, q.reference_time.as_deref())?].1.levels.clone();
    let levels_match = |levels: &[f32]| {
        levels.len() == reference_levels.len()
            && levels.iter().zip(&reference_levels).all(|(a, b)| (a - b).abs() < 0.01)
    };
    let usable: Vec<_> = volumes
        .iter()
        .filter(|(t, v, _)| {
            let ok = levels_match(&v.levels);
            if !ok {
                excluded.push(json!({"analysis_time": t, "reason": "CAPPI level grid differs from the reference analysis"}));
            }
            ok
        })
        .collect();

    let times: Vec<String> = usable.iter().map(|(t, _, _)| t.clone()).collect();
    let centers: Vec<&AnalysisCenter> = usable.iter().map(|(_, _, c)| c).collect();
    let reference = reference_index(&times, q.reference_time.as_deref())?;
    let n_levels = reference_levels.len();
    let combine_mode = sweep::combine_mode_for_field(&q.field);
    let mut mosaic_x = Vec::new();
    let mut mosaic_y = Vec::new();
    let mut data_out: Vec<Vec<Vec<Option<f64>>>> = Vec::with_capacity(n_levels);
    for li in 0..n_levels {
        let planes: Vec<sweep::StormPlane> = usable
            .iter()
            .map(|(_, v, c)| sweep::StormPlane {
                x: &v.x,
                y: &v.y,
                data: &v.data[li],
                center_x_km: c.x_km,
                center_y_km: c.y_km,
            })
            .collect();
        let mosaic = sweep::storm_centered_mosaic(&planes, reference, combine_mode);
        if li == 0 {
            mosaic_x = mosaic.x;
            mosaic_y = mosaic.y;
        }
        data_out.push(mosaic.data.iter().map(|row| row.iter().map(|v| v.map(|x| x as f64)).collect()).collect());
    }

    let detail = centering_detail(&times, &centers, reference, &excluded, &unsuitable, combine_mode);
    let r = centers[reference];
    let cs = colorscale_for_field(&q.field);
    let mut response = json!({
        "mission_id": mission.mission_id,
        "storm_name": mission.storm_name,
        "level": level,
        "product": q.product,
        "field": q.field,
        "mode": "time_volume",
        "detail": detail,
        "x": mosaic_x,
        "y": mosaic_y,
        "levels_km": reference_levels,
        "data": data_out,
        "origin_lat": r.lat,
        "origin_lon": r.lon,
        "colorscale": cs.stops,
        "zmin": cs.zmin,
        "zmax": cs.zmax,
        "units": cs.units,
    });
    insert_qc_fields(&mut response, want_qc.then_some(qc_report), qc_params);
    Ok(Json(response))
}

#[derive(Deserialize)]
struct PlaneSliceQuery {
    mission_id: String,
    level: Option<String>,
    /// `xy` or `xy_rel` only — a plane slice needs the whole level axis of
    /// a volume to cut through, same restriction as `GET /v1/tdr/volume`.
    product: String,
    analysis_time: String,
    field: String,
    /// The cut line's two endpoints, in the same km-from-origin coordinate
    /// system as the `x`/`y` a sweep/volume response returns — i.e. exactly
    /// the coordinates a client already has in hand from a prior sweep or
    /// volume plot, so it can let a user click two points on that plot and
    /// pass them straight through.
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    /// How many evenly-spaced points to sample along the line. Defaults to
    /// 100; clamped to at least 2.
    n: Option<usize>,
    /// Same meaning as `SweepQuery::qc` — runs on the whole volume before
    /// the cut is taken, so the cross-section reflects cleaned data rather
    /// than being interpolated from a still-raw volume.
    qc: Option<bool>,
}

/// `GET /v1/tdr/plane_slice` — the "plane slice" tool: an arbitrary
/// vertical cross-section through one analysis time's `xy` volume, cut
/// between two points a user picks on the horizontal CAPPI image, rather
/// than only the fixed along/across-track cuts baked into
/// `vert_inbound`/`vert_outbound`. Reads the whole volume (every CAPPI
/// level, same as `GET /v1/tdr/volume`) and bilinearly interpolates each
/// level along the requested line
/// ([`noaa_recon_core::sweep::plane_slice`]), so the cross-section is
/// smooth regardless of the line's angle through the grid — not just
/// snapped to the nearest existing column.
async fn get_plane_slice(
    State(state): State<AppState>,
    Query(q): Query<PlaneSliceQuery>,
    Query(tuning): Query<QcTuning>,
) -> ApiResult<Json<Value>> {
    let want_qc = q.qc.unwrap_or(false);
    let qc_params = tuning.resolve(want_qc)?;
    let conn = conn(&state)?;
    let (mission, file, level) =
        resolve_mission_and_file(&conn, &q.mission_id, &q.level, &q.product, &q.analysis_time, want_qc)?;
    let counterpart_file =
        if want_qc { tdr::find_file(&conn, &q.mission_id, &level, "xy_rel", &q.analysis_time, "nc")? } else { None };

    let cache_dir = state.paths.cache_root.join("tdr_nc");
    let cache_key = format!("{}_{level}_{}_{}", q.mission_id, q.product, q.analysis_time);
    let nc_path = tdr_nc::fetch_and_cache(&cache_dir, &file.source_url, &cache_key)
        .await
        .map_err(|e| ApiError::bad_gateway(format!("Failed to fetch/decompress source file: {e}")))?;

    let counterpart = match &counterpart_file {
        Some(cf) => Some(fetch_qc_counterpart_volume(&cache_dir, cf, &q.mission_id, &level, &q.analysis_time, &q.field).await?),
        None => None,
    };

    let field = q.field.clone();
    let (mut volume, wind) = tokio::task::spawn_blocking(move || {
        let volume = tdr_nc::read_xy_volume(&nc_path, &field)?;
        Ok::<_, anyhow::Error>((volume, tdr_nc::read_qc_wind_volume(&nc_path, &field, qc_params)?))
    })
    .await
    .map_err(|e| ApiError::internal(format!("volume read task panicked: {e}")))?
    .map_err(|e| ApiError::bad_request(e.to_string()))?;

    let qc_report = if want_qc {
        Some(tdr_nc::apply_qc_to_volume(&mut volume, &q.field, counterpart.as_ref(), wind.as_ref(), &qc_params.unwrap_or_default()))
    } else {
        None
    };

    let n = q.n.unwrap_or(100).max(2);
    let cut = noaa_recon_core::sweep::plane_slice(
        &volume.data,
        &volume.x,
        &volume.y,
        &volume.levels,
        q.x0,
        q.y0,
        q.x1,
        q.y1,
        n,
    );

    let cs = colorscale_for_field(&q.field);
    let data: Vec<Vec<Option<f64>>> =
        cut.data.iter().map(|row| row.iter().map(|v| v.map(|x| x as f64)).collect()).collect();

    let mut response = json!({
        "mission_id": mission.mission_id,
        "storm_name": volume.storm_name_attr.unwrap_or(mission.storm_name),
        "level": level,
        "product": q.product,
        "analysis_time": q.analysis_time,
        "field": q.field,
        "endpoints": {"x0": q.x0, "y0": q.y0, "x1": q.x1, "y1": q.y1},
        "x": cut.along_km,
        "y": cut.levels,
        "data": data,
        "colorscale": cs.stops,
        "zmin": cs.zmin,
        "zmax": cs.zmax,
        "units": cs.units,
        "origin_lat": volume.origin_lat,
        "origin_lon": volume.origin_lon,
    });
    insert_qc_fields(&mut response, qc_report, qc_params);
    Ok(Json(response))
}

#[derive(Deserialize)]
struct CentersQuery {
    mission_id: String,
    /// `"1b"` or `"2"` — same default rule as `SweepQuery::level`.
    level: Option<String>,
    /// `xy` or `xy_rel` — the center is derived from the wind volume, so a
    /// vertical profile (no level axis) isn't valid. Defaults to `xy`.
    product: Option<String>,
    analysis_time: String,
    /// Annulus (km) the azimuthal-mean tangential wind is evaluated over —
    /// should bracket the radius of maximum wind. Default 2–50 km.
    rmin_km: Option<f32>,
    rmax_km: Option<f32>,
    /// How far (km) from the grid origin the search may wander. Default 50.
    max_offset_km: Option<f32>,
    /// A level whose best azimuthal-mean tangential wind is below this (m/s)
    /// is reported center-less. Default 3.
    min_tangential_wind_ms: Option<f32>,
    /// Level-to-level continuity radius (km): each level is searched within
    /// this distance of the neighbouring level's accepted center. Default 12.
    continuity_km: Option<f32>,
}

/// `GET /v1/tdr/centers` — the TDR-derived storm center at *each* CAPPI
/// altitude for one analysis time. The gridded synthesis only stores one
/// `ORIGIN_LATITUDE/LONGITUDE` for the whole volume, but a real vortex tilts
/// with height, so this recomputes the center level-by-level from the analysis
/// `U`/`V` wind field: at each level, the point that maximizes the azimuthal-
/// mean tangential wind — the most symmetric cyclonic circulation, HRD's
/// center criterion (see [`noaa_recon_core::sweep::tangential_wind_centers`]).
/// Returns `[{level_km, lat, lon, x_km, y_km, tangential_wind_ms, rmw_km}]`,
/// center fields `null` at levels with no coherent circulation.
async fn get_centers(State(state): State<AppState>, Query(q): Query<CentersQuery>) -> ApiResult<Json<Value>> {
    let product = q.product.clone().unwrap_or_else(|| "xy".into());
    let conn = tdr::get_connection(&state.paths.tdr_db)?;
    // Custom QC isn't wired up for center-finding — not exposed in the
    // dashboard, and this endpoint's raw U/V fit is arguably a different
    // concern from the gridded-field QC checks here — so `qc` is always
    // `false` for this call.
    let (mission, file, level) =
        resolve_mission_and_file(&conn, &q.mission_id, &q.level, &product, &q.analysis_time, false)?;
    drop(conn);

    let cache_dir = state.paths.cache_root.join("tdr_nc");
    let cache_key = format!("{}_{level}_{}_{}", q.mission_id, product, q.analysis_time);
    let nc_path = tdr_nc::fetch_and_cache(&cache_dir, &file.source_url, &cache_key)
        .await
        .map_err(|e| ApiError::bad_gateway(format!("Failed to fetch/decompress source file: {e}")))?;

    let params = noaa_recon_core::sweep::CenterParams {
        rmin_km: q.rmin_km.unwrap_or(2.0),
        rmax_km: q.rmax_km.unwrap_or(50.0),
        max_offset_km: q.max_offset_km.unwrap_or(50.0),
        min_vtan_ms: q.min_tangential_wind_ms.unwrap_or(3.0),
        min_points: 30,
        continuity_km: q.continuity_km.unwrap_or(12.0),
    };

    // Read both wind volumes and locate the centers off-thread (netCDF decode +
    // the per-level search are blocking, non-Send work).
    let (centers, origin_lat, origin_lon, storm_attr) = tokio::task::spawn_blocking(move || {
        let u = tdr_nc::read_xy_volume(&nc_path, "u")?;
        let v = tdr_nc::read_xy_volume(&nc_path, "v")?;
        let centers =
            noaa_recon_core::sweep::tangential_wind_centers(&u.data, &v.data, &u.x, &u.y, &u.levels, &params);
        Ok::<_, anyhow::Error>((centers, u.origin_lat, u.origin_lon, u.storm_name_attr))
    })
    .await
    .map_err(|e| ApiError::internal(format!("center-finding task panicked: {e}")))?
    .map_err(|e| ApiError::bad_request(e.to_string()))?;

    let centers_json: Vec<Value> = centers
        .iter()
        .map(|c| {
            let (lat, lon) = match (c.x_km, c.y_km, origin_lat, origin_lon) {
                (Some(x), Some(y), Some(la), Some(lo)) => {
                    let (lat, lon) = noaa_recon_core::sweep::latlon_from_offset_km(x, y, la, lo);
                    (Some(lat), Some(lon))
                }
                _ => (None, None),
            };
            json!({
                "level_km": c.level_km,
                "lat": lat,
                "lon": lon,
                "x_km": c.x_km,
                "y_km": c.y_km,
                "tangential_wind_ms": c.vtan_ms,
                "rmw_km": c.rmw_km,
            })
        })
        .collect();

    Ok(Json(json!({
        "mission_id": mission.mission_id,
        "storm_name": storm_attr.unwrap_or(mission.storm_name),
        "product": product,
        "level": level,
        "analysis_time": q.analysis_time,
        "method": "per-level center maximizing azimuthal-mean tangential wind (from U/V)",
        "origin_lat": origin_lat,
        "origin_lon": origin_lon,
        "params": {
            "rmin_km": params.rmin_km,
            "rmax_km": params.rmax_km,
            "max_offset_km": params.max_offset_km,
            "min_tangential_wind_ms": params.min_vtan_ms,
            "continuity_km": params.continuity_km,
        },
        "centers": centers_json,
    })))
}
