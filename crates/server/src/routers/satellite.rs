//! Port of `app/routers/satellite.py`.
//!
//! Discovery endpoints (`/products`, `/colortables`, `/colortable`, `/status`)
//! are fully ported — they need no netCDF, only the `core` colormap/catalog.
//! `/tile` (which downloads + decodes + renders GOES imagery) returns 501 until
//! the netCDF decode + render pipeline lands; the validation/cache scaffolding
//! is in place for when it does.

use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use chrono::{DateTime, Utc};

use noaa_recon_core::bbox::resolve_bbox_request;
use noaa_recon_core::catalog;
use noaa_recon_core::colormap;

use crate::error::{ApiError, ApiResult};
use crate::services::cache::ResultCache;
use crate::services::goes;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/satellite/tile", get(get_tile))
        .route("/satellite/status/:key", get(get_status))
        .route("/satellite/colortable", get(get_colortable))
        .route("/satellite/colortables", get(list_colortables))
        .route("/satellite/products", get(list_products))
}

fn sat_cache(state: &AppState) -> ApiResult<ResultCache> {
    ResultCache::new(state.paths.cache_root.join("satellite"), 600)
        .map_err(|e| ApiError::internal(format!("cache init: {e}")))
}

fn rgb_to_hex(rgb: colormap::Rgb) -> String {
    format!("#{:02X}{:02X}{:02X}", rgb[0], rgb[1], rgb[2])
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

// ── /tile ───────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct TileQuery {
    time: Option<String>,
    #[serde(default)]
    band: Option<i64>,
    #[serde(default)]
    cmap: Option<String>,
    product: Option<String>,
    #[serde(default)]
    satellite: Option<String>,
    center: Option<String>,
    dims: Option<f64>,
    #[serde(default)]
    unit: Option<String>,
    resolution_km: Option<f64>,
}

fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    // FastAPI accepts ISO with or without timezone; a naive time is treated as UTC.
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S")
        .ok()
        .map(|n| DateTime::<Utc>::from_naive_utc_and_offset(n, Utc))
}

fn parse_center(center: &str) -> Result<(f64, f64), ApiError> {
    let parts: Vec<&str> = center.split(',').collect();
    if parts.len() != 2 {
        return Err(ApiError::bad_request("center must be 'lat,lon', e.g. '25.5,-80.3'"));
    }
    let lat = parts[0].trim().parse::<f64>();
    let lon = parts[1].trim().parse::<f64>();
    match (lat, lon) {
        (Ok(la), Ok(lo)) => Ok((la, lo)),
        _ => Err(ApiError::bad_request("center must be 'lat,lon' with numeric values")),
    }
}

async fn get_tile(State(state): State<AppState>, Query(q): Query<TileQuery>) -> ApiResult<Json<Value>> {
    let time_str = q.time.ok_or_else(|| ApiError::bad_request("time query parameter is required"))?;
    let time = parse_time(&time_str)
        .ok_or_else(|| ApiError::bad_request(format!("time must be ISO 8601: {time_str}")))?;

    let satellite = q.satellite.unwrap_or_else(|| "goes-east".into());
    if satellite != "goes-east" && satellite != "goes-west" {
        return Err(ApiError::bad_request("satellite must be 'goes-east' or 'goes-west'"));
    }
    let sat_side = if satellite == "goes-west" { "west" } else { "east" };
    let unit = q.unit.unwrap_or_else(|| "nm".into());
    if unit != "nm" && unit != "km" {
        return Err(ApiError::bad_request("unit must be 'nm' or 'km'"));
    }
    if q.center.is_some() != q.dims.is_some() {
        return Err(ApiError::bad_request(
            "center and dims must be provided together (omit both for a full-disk render)",
        ));
    }

    let cache = sat_cache(&state)?;
    let nc_cache_dir = state.paths.cache_root.join("goes_nc");

    if let Some(product) = q.product.as_deref() {
        if !catalog::is_valid_product(product) {
            return Err(invalid_product());
        }

        let bbox = match (q.center.as_deref(), q.dims) {
            (Some(center), Some(dims)) => {
                let (lat, lon) = parse_center(center)?;
                let width_km = if unit == "km" { dims } else { dims * 1.852 };
                // band=2 here only picks which band's native GSD floors the
                // resolution clamp — Band 2 (0.5km) is the finest of any band
                // either composite uses, so a bbox request can go as sharp
                // as the sharpest input actually supports.
                let b = resolve_bbox_request(lat, lon, width_km, q.resolution_km, 2)
                    .map_err(ApiError::bad_request)?;
                Some(b)
            }
            _ => None,
        };

        let resolved_ir = goes::resolve_nearest(time, 13, sat_side)
            .await
            .map_err(|e| ApiError::bad_gateway(format!("S3 resolve failed: {e}")))?
            .ok_or_else(|| {
                ApiError::not_found(format!("No GOES-{sat_side} Band 13 scan found near {}", time.to_rfc3339()))
            })?;

        let scan_stamp = resolved_ir.scan_start.format("%Y%m%dT%H%M%S");
        let mut key = format!("goes_{product}_{}_{scan_stamp}", resolved_ir.satellite);
        if let Some(b) = &bbox {
            key += &format!(
                "_c{:.3}_{:.3}_w{:.0}_r{:.1}",
                b.center_lat, b.center_lon, b.width_km, b.resolution_km
            );
        }

        if let Some(status) = cache.get_status(&key) {
            return Ok(Json(status));
        }

        let mut lock_params = json!({
            "product": product,
            "satellite": format!("GOES-{}", resolved_ir.satellite),
            "scan_start": resolved_ir.scan_start.to_rfc3339(),
        });
        if let Some(b) = &bbox {
            let m = lock_params.as_object_mut().unwrap();
            m.insert("center".into(), json!([b.center_lat, b.center_lon]));
            m.insert("width_km".into(), json!(b.width_km));
        }
        cache
            .acquire_lock(&key, Some(&lock_params))
            .map_err(|e| ApiError::internal(format!("cache lock: {e}")))?;

        let task_cache = sat_cache(&state)?;
        tokio::spawn(goes::render_product_and_store(
            product.to_string(),
            resolved_ir,
            key.clone(),
            nc_cache_dir,
            task_cache,
            bbox,
        ));

        let mut resp = lock_params;
        let m = resp.as_object_mut().unwrap();
        m.insert("status".into(), json!("generating"));
        m.insert("key".into(), json!(key));
        return Ok(Json(resp));
    }

    let band = q.band.unwrap_or(13);
    if !catalog::is_valid_band(band) {
        return Err(invalid_band());
    }
    let mut cmap = q.cmap.unwrap_or_else(|| "default".into());
    if !catalog::is_valid_cmap(&cmap) {
        return Err(invalid_cmap());
    }
    if cmap == "default" {
        cmap = catalog::default_cmap_by_band(band).unwrap().to_string();
    }

    // Resolve the optional bbox.
    let bbox = match (q.center.as_deref(), q.dims) {
        (Some(center), Some(dims)) => {
            let (lat, lon) = parse_center(center)?;
            let width_km = if unit == "km" { dims } else { dims * 1.852 };
            let b = resolve_bbox_request(lat, lon, width_km, q.resolution_km, band)
                .map_err(ApiError::bad_request)?;
            Some(b)
        }
        _ => None,
    };

    // Resolve the nearest scan (S3 listing).
    let resolved = goes::resolve_nearest(time, band, sat_side)
        .await
        .map_err(|e| ApiError::bad_gateway(format!("S3 resolve failed: {e}")))?
        .ok_or_else(|| {
            ApiError::not_found(format!(
                "No GOES-{sat_side} Band {band} scan found near {}",
                time.to_rfc3339()
            ))
        })?;

    // Cache key mirrors the Python's exactly.
    let scan_stamp = resolved.scan_start.format("%Y%m%dT%H%M%S");
    let mut key = format!("goes_{band}_{cmap}_{}_{scan_stamp}", resolved.satellite);
    if let Some(b) = &bbox {
        key += &format!(
            "_c{:.3}_{:.3}_w{:.0}_r{:.1}",
            b.center_lat, b.center_lon, b.width_km, b.resolution_km
        );
    }

    if let Some(status) = cache.get_status(&key) {
        return Ok(Json(status));
    }

    let mut lock_params = json!({
        "band": band,
        "cmap": cmap,
        "satellite": format!("GOES-{}", resolved.satellite),
        "scan_start": resolved.scan_start.to_rfc3339(),
    });
    if let Some(b) = &bbox {
        let m = lock_params.as_object_mut().unwrap();
        m.insert("center".into(), json!([b.center_lat, b.center_lon]));
        m.insert("width_km".into(), json!(b.width_km));
    }
    cache
        .acquire_lock(&key, Some(&lock_params))
        .map_err(|e| ApiError::internal(format!("cache lock: {e}")))?;

    // Kick the render in the background (FastAPI BackgroundTask -> tokio::spawn).
    let task_cache = sat_cache(&state)?;
    tokio::spawn(goes::render_and_store(
        resolved,
        cmap.clone(),
        key.clone(),
        nc_cache_dir,
        task_cache,
        bbox,
    ));

    let mut resp = lock_params;
    let m = resp.as_object_mut().unwrap();
    m.insert("status".into(), json!("generating"));
    m.insert("key".into(), json!(key));
    Ok(Json(resp))
}

// ── /status/{key} ─────────────────────────────────────────────────────────────

async fn get_status(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> ApiResult<Json<Value>> {
    let cache = sat_cache(&state)?;
    match cache.get_status(&key) {
        Some(status) => Ok(Json(status)),
        None => Ok(Json(json!({ "status": "idle" }))),
    }
}

// ── /colortable ───────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ColortableQuery {
    #[serde(default = "default_cmap")]
    cmap: String,
    #[serde(default = "default_band")]
    band: i64,
}
fn default_cmap() -> String {
    "default".to_string()
}
fn default_band() -> i64 {
    13
}

async fn get_colortable(Query(q): Query<ColortableQuery>) -> ApiResult<Json<Value>> {
    let mut cmap = q.cmap;
    if !catalog::is_valid_cmap(&cmap) {
        return Err(invalid_cmap());
    }
    if cmap == "default" {
        if !catalog::is_valid_band(q.band) {
            return Err(invalid_band());
        }
        cmap = catalog::default_cmap_by_band(q.band).unwrap().to_string();
    }

    if catalog::is_reflectance_cmap(&cmap) {
        // Reflectance ramp legend: 0..=100% grayscale through the gamma stretch.
        let stops: Vec<Value> = (0..=100)
            .step_by(10)
            .map(|pct| {
                let v = colormap::reflectance_gray(pct as f64 / 100.0, 1.5);
                json!({ "reflectance_pct": pct, "hex": rgb_to_hex([v, v, v]) })
            })
            .collect();
        return Ok(Json(json!({ "cmap": cmap, "unit": "%", "exact": true, "stops": stops })));
    }

    if let Some(stops_tbl) = colormap::stops_by_cmap(&cmap) {
        let stops: Vec<Value> = stops_tbl
            .iter()
            .map(|(t, rgb)| json!({ "temp_c": t, "hex": rgb_to_hex(*rgb) }))
            .collect();
        return Ok(Json(json!({ "cmap": cmap, "unit": "C", "exact": true, "stops": stops })));
    }

    // LUT-based: sample every 16th index for a representative legend.
    let lut = colormap::build_lut(&cmap)
        .ok_or_else(|| ApiError::internal(format!("no LUT for cmap {cmap}")))?;
    let mut stops: Vec<(f64, String)> = (0..256)
        .step_by(16)
        .map(|i| (round1(colormap::i2t(i) - 273.15), rgb_to_hex(lut[i])))
        .collect();
    stops.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    let stops_json: Vec<Value> = stops
        .into_iter()
        .map(|(t, hex)| json!({ "temp_c": t, "hex": hex }))
        .collect();
    Ok(Json(json!({ "cmap": cmap, "unit": "C", "exact": false, "stops": stops_json })))
}

// ── /colortables ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ColortablesQuery {
    band: Option<i64>,
    product: Option<String>,
}

async fn list_colortables(Query(q): Query<ColortablesQuery>) -> ApiResult<Json<Value>> {
    if let Some(product) = q.product {
        if q.band.is_some() {
            return Err(ApiError::bad_request("pass either `band` or `product`, not both"));
        }
        if !catalog::is_valid_product(&product) {
            return Err(invalid_product());
        }
        let mut entry = cmap_description("abi13");
        let obj = entry.as_object_mut().unwrap();
        obj.insert("cmap".into(), json!("abi13"));
        obj.insert("is_default".into(), json!(true));
        obj.insert("kind".into(), json!("brightness_temp"));
        obj.insert("unit".into(), json!("C"));
        return Ok(Json(json!({
            "product": product,
            "colortables": [entry],
            "note": "Composite products always use the abi13 IR enhancement for their thermal component \
                     (daytime pixels are true-color/blended, not on this scale) — `cmap` is not selectable for `product` requests.",
        })));
    }

    let band = q.band.unwrap_or(13);
    if !catalog::is_valid_band(band) {
        return Err(invalid_band());
    }
    let default_cmap = catalog::default_cmap_by_band(band).unwrap();
    let is_reflectance = catalog::is_reflectance_band(band);
    let kind = if is_reflectance { "reflectance" } else { "brightness_temp" };

    // sorted({default_cmap} | (LUT names unless reflectance))
    let mut cmaps: Vec<String> = vec![default_cmap.to_string()];
    if !is_reflectance {
        for n in colormap::LUT_NAMES {
            cmaps.push(n.to_string());
        }
    }
    cmaps.sort();
    cmaps.dedup();

    let colortables: Vec<Value> = cmaps
        .iter()
        .map(|cmap| {
            let mut entry = cmap_description(cmap);
            let entry_kind = if catalog::is_reflectance_cmap(cmap) {
                "reflectance"
            } else {
                "brightness_temp"
            };
            let obj = entry.as_object_mut().unwrap();
            obj.insert("cmap".into(), json!(cmap));
            obj.insert("is_default".into(), json!(cmap == default_cmap));
            obj.insert("kind".into(), json!(entry_kind));
            obj.insert("unit".into(), json!(if entry_kind == "reflectance" { "%" } else { "C" }));
            entry
        })
        .collect();

    Ok(Json(json!({
        "band": band,
        "kind": kind,
        "default_cmap": default_cmap,
        "colortables": colortables,
    })))
}

// ── /products ─────────────────────────────────────────────────────────────────

async fn list_products() -> Json<Value> {
    let mut valid_bands = catalog::VALID_BANDS;
    valid_bands.sort();
    let bands: Vec<Value> = valid_bands
        .iter()
        .map(|&band| {
            let default_cmap = catalog::default_cmap_by_band(band).unwrap();
            let is_reflectance = catalog::is_reflectance_band(band);
            let mut cmaps: Vec<String> = vec![default_cmap.to_string()];
            if !is_reflectance {
                for n in colormap::LUT_NAMES {
                    cmaps.push(n.to_string());
                }
            }
            cmaps.sort();
            cmaps.dedup();
            json!({
                "band": band,
                "name": band_name(band),
                "kind": if is_reflectance { "reflectance" } else { "brightness_temp" },
                "default_cmap": default_cmap,
                "cmaps": cmaps,
                "native_resolution_km": catalog::native_gsd_km(band),
                "bbox_supported": true,
            })
        })
        .collect();

    let products = json!([
        {
            "product": "sandwich",
            "name": "IR/VIS Sandwich",
            "description": "Band 13 IR colorized with the abi13 enhancement, modulated by Band 2 visible \
                brightness to show convective texture. Falls back to darkened plain IR at night (no visible signal).",
            "bbox_supported": true,
        },
        {
            "product": "geocolor",
            "name": "GeoColor-style composite (approximate)",
            "description": "Simplified day/night composite: synthetic true color (Bands 1/2/3, CIRA synthetic-\
                green recipe) by day, abi13 colorized IR by night, blended by solar zenith angle near the \
                terminator. NOT NOAA/CIRA's proprietary GeoColor — no city lights layer, no atmospheric \
                (Rayleigh) correction.",
            "bbox_supported": true,
        }
    ]);

    Json(json!({ "bands": bands, "products": products, "satellites": satellite_coverage() }))
}

// ── Presentation-layer text (kept out of core) ───────────────────────────────

fn band_name(band: i64) -> &'static str {
    match band {
        2 => "Red (Visible), 0.64µm",
        3 => "Veggie (Vegetation/NIR), 0.86µm",
        5 => "Near-IR (Snow/Ice), 1.6µm",
        7 => "Shortwave IR (\"Fire Temperature\"), 3.9µm",
        9 => "Mid-Level Water Vapor, 6.9µm",
        13 => "Clean IR Window, 10.3µm",
        _ => "",
    }
}

/// `{"name":..,"description":..}` per cmap, matching `CMAP_DESCRIPTIONS`.
fn cmap_description(cmap: &str) -> Value {
    let (name, desc): (&str, &str) = match cmap {
        "abi13" => ("Band 13 Standard Enhancement",
            "White at the most extreme cold overshooting tops (-110C) down through black (-80C), a rainbow band -80C to -32C highlighting severe convection, a hard cut to light grey at -31C, then greyscale (light=cold, dark=warm) to black at +57C."),
        "abi9" => ("Band 9 (Water Vapor) Standard Enhancement",
            "Cyan at coldest/moist (-93C) through green tones, white at the moist/dry transition (-42C), a purple/navy/indigo band (-30C to -18C), then yellow-orange-red to black at warmest/driest (+7C)."),
        "abi7" => ("Band 7 (Shortwave IR / Fire Temperature) Standard Enhancement",
            "Greyscale over the same cloud-top range as 9/13, then a yellow-red highlight above normal clear-sky warmth (~+57C) to flag hotspots — this band saturates far higher than 9/13."),
        "abi5" => ("Band 5 (Near-IR Snow/Ice) Reflectance Ramp",
            "Not a temperature colortable — reports reflectance factor (~0-1), rendered as a gamma-stretched 0-100% grayscale."),
        "abi2" => ("Band 2 (Red/Visible) Reflectance Ramp",
            "Same treatment as abi5/abi3 — reflectance, not temperature, rendered as a gamma-stretched grayscale. The sharpest band this API renders (0.5km native) — daylight-only, no signal at night."),
        "abi3" => ("Band 3 (Veggie / Vegetation-NIR) Reflectance Ramp",
            "Same treatment as abi5 — reflectance, not temperature, rendered as a gamma-stretched grayscale. Sensitive to chlorophyll/vegetation reflectance."),
        "ir4" => ("IR4 (satpy colorized_ir_clouds)",
            "An alternate Band 13 enhancement sourced from satpy: greyscale -20C to +30C, then the ColorBrewer 'Spectral' 11-class diverging palette -80C to -20C. Kept for comparison — abi13 is the recommended default for Band 13."),
        "bd" => ("NWS/Dvorak BD Enhancement",
            "Standard NWS/Dvorak BD enhancement — greyscale for warm/moderate tops, blue-purple-red for cold convection."),
        "enhanced" => ("Enhanced", "Darker surface/low clouds, white mid/high clouds, color for coldest tops."),
        "nrl" => ("NRL Tropical Cyclone",
            "Naval Research Lab tropical cyclone enhancement — smooth yellow-green to cyan to blue to purple to red ramp."),
        "grayscale" => ("Grayscale", "Plain linear greyscale by brightness temperature."),
        other => return json!({ "name": other, "description": "" }),
    };
    json!({ "name": name, "description": desc })
}

fn satellite_coverage() -> Value {
    json!({
        "goes-east": [
            {"satellite": "GOES-16", "start": "2017-12-18", "end": "2025-01-14"},
            {"satellite": "GOES-19", "start": "2025-01-14", "end": null}
        ],
        "goes-west": [
            {"satellite": "GOES-17", "start": "2019-02-12", "end": "2023-01-10"},
            {"satellite": "GOES-18", "start": "2023-01-10", "end": null}
        ]
    })
}

fn invalid_cmap() -> ApiError {
    ApiError::bad_request(format!("cmap must be one of {:?}", sorted(catalog::valid_cmaps())))
}
fn invalid_band() -> ApiError {
    let mut b = catalog::VALID_BANDS.to_vec();
    b.sort();
    ApiError::bad_request(format!("band must be one of {b:?}"))
}
fn invalid_product() -> ApiError {
    ApiError::bad_request(format!("product must be one of {:?}", sorted(catalog::VALID_PRODUCTS.to_vec())))
}
fn sorted<T: Ord>(mut v: Vec<T>) -> Vec<T> {
    v.sort();
    v
}
