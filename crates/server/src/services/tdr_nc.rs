//! TDR netCDF fetch + decode — the server-side half of `GET /v1/tdr/sweep`
//! (the pure slicing math lives in `noaa_recon_core::sweep`, WASM-safe).
//!
//! Unlike `tdr_ingest.rs`, this module *does* download a file — but only the
//! one file a real request actually asks for, lazily on first request,
//! cached under `cache/tdr_nc/` forever after. Same on-demand-cache
//! principle as `cache/goes_nc/` in `goes.rs`, just without that module's
//! job/poll ceremony: a `.gz` here is a few MB at most (an `xy.nc.gz` is
//! ~5-7MB, `vert_*.nc.gz` under 150KB), much smaller than GOES's ~25MB S3
//! fetches or the recon archive's ~85MB full-resolution downloads — and the
//! recon archive's own `/mission/{id}/download` streams synchronously with
//! no job/poll wrapper either, for the same reason. Revisit if a slower
//! decode/slice step gets added later.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use noaa_recon_core::sweep;

use crate::services::goes::nc_lock;

const DEFAULT_MISSING: f32 = -999.9;

/// xy-volume field name -> netCDF variable name (all dims `x,y,level,time`).
const XY_FIELD_VARS: &[(&str, &str)] = &[
    ("reflectivity", "REFLECTIVITY"),
    ("radial_wind", "RADIAL_WIND"),
    ("tangential_wind", "TANGENTIAL_WIND"),
    ("u", "U"),
    ("v", "V"),
    ("w", "W"),
    ("vort", "VORT"),
    ("wind_speed", "WIND_SPEED"),
];

/// Vertical-profile field name -> netCDF variable name (all dims
/// `radius,heading,height,time`) — note the mixed-case variable names here,
/// genuinely different from the xy volume's all-caps convention (both
/// confirmed against real files, not a typo).
const VERT_FIELD_VARS: &[(&str, &str)] = &[
    ("reflectivity", "REFLECTIVITY"),
    ("radial_wind", "Radial_wind"),
    ("tangential_wind", "Tangential_Wind"),
    ("wind_speed", "Wind_Speed"),
];

pub fn xy_field_names() -> Vec<&'static str> {
    XY_FIELD_VARS.iter().map(|(k, _)| *k).collect()
}

pub fn vert_field_names() -> Vec<&'static str> {
    VERT_FIELD_VARS.iter().map(|(k, _)| *k).collect()
}

fn resolve_var_name(table: &[(&'static str, &'static str)], field: &str) -> Option<&'static str> {
    table.iter().find(|(k, _)| *k == field).map(|(_, v)| *v)
}

pub struct FieldSlice {
    /// Column coordinate: km-from-origin for an xy CAPPI, along-track radius
    /// (km) for a vertical profile.
    pub x: Vec<f32>,
    /// Row coordinate: km-from-origin for an xy CAPPI, height (km) for a
    /// vertical profile.
    pub y: Vec<f32>,
    pub data: Vec<Vec<Option<f32>>>,
    /// Resolved CAPPI altitude (xy product only — `None` for a vertical
    /// profile or an altitude composite, which spans every level).
    pub z_km: Option<f32>,
    pub origin_lat: Option<f32>,
    pub origin_lon: Option<f32>,
    pub storm_name_attr: Option<String>,
}

/// Every CAPPI plane of an `xy`/`xy_rel` volume, for genuine 3D rendering —
/// same grid/origin/attrs as [`FieldSlice`] but `data[level_idx][yi][xi]`
/// plus the actual `levels` (km) each plane sits at.
pub struct FieldVolume {
    pub x: Vec<f32>,
    pub y: Vec<f32>,
    pub levels: Vec<f32>,
    pub data: Vec<Vec<Vec<Option<f32>>>>,
    pub origin_lat: Option<f32>,
    pub origin_lon: Option<f32>,
    pub storm_name_attr: Option<String>,
}

/// Shared open+validate+read step for `xy`/`xy_rel` volume files — used by
/// [`read_xy_slice`], [`read_xy_volume`], and [`read_xy_altitude_composite`]
/// so the three only differ in what they do with `flat`.
struct XyVolumeRaw {
    x: Vec<f32>,
    y: Vec<f32>,
    levels: Vec<f32>,
    flat: Vec<f32>,
    missing: f32,
    origin_lat: Option<f32>,
    origin_lon: Option<f32>,
    storm_name_attr: Option<String>,
}

fn read_xy_volume_raw(path: &Path, field: &str) -> anyhow::Result<XyVolumeRaw> {
    let var_name = resolve_var_name(XY_FIELD_VARS, field)
        .ok_or_else(|| anyhow::anyhow!("unknown xy field '{field}', expected one of {:?}", xy_field_names()))?;

    let _guard = nc_lock();
    let ds = netcdf::open(path)?;
    let origin_lat = global_attr_f32(&ds, "ORIGIN_LATITUDE");
    let origin_lon = global_attr_f32(&ds, "ORIGIN_LONGITUDE");

    let (x, y) = if ds.variable("x").is_some() && ds.variable("y").is_some() && ds.variable("LATITUDE").is_some() {
        // Post-2021 schema: Cartesian x/y (km-from-center) declared directly.
        let x: Vec<f32> = ds.variable("x").unwrap().get_values(..)?;
        let y: Vec<f32> = ds.variable("y").unwrap().get_values(..)?;
        (x, y)
    } else if ds.variable("lons").is_some() && ds.variable("lats").is_some() {
        // Pre-2021 schema (see the AOML TDR README's 2021 gridding-change
        // note): grid is regularly spaced in degrees lat/lon instead of a
        // declared Cartesian x/y. Field variables still use dims `(lons,
        // lats, level, time)` — the same slowest-to-fastest order as
        // post-2021's `(x, y, level, time)` — so we only need to turn `lons`/
        // `lats` into an equivalent km-from-origin grid for `sweep`'s slicing
        // math and for `geo_mosaic` alignment against post-2021 volumes.
        let (lat0, lon0) = origin_lat.zip(origin_lon).ok_or_else(|| {
            anyhow::anyhow!(
                "pre-2021 lat/lon-gridded xy file is missing ORIGIN_LATITUDE/ORIGIN_LONGITUDE, \
                 needed to convert its lons/lats grid to km-from-origin"
            )
        })?;
        let lons: Vec<f32> = ds.variable("lons").unwrap().get_values(..)?;
        let lats: Vec<f32> = ds.variable("lats").unwrap().get_values(..)?;
        let x: Vec<f32> = lons.iter().map(|&lon| sweep::latlon_offset_km(lat0, lon, lat0, lon0).0).collect();
        let y: Vec<f32> = lats.iter().map(|&lat| sweep::latlon_offset_km(lat, lon0, lat0, lon0).1).collect();
        (x, y)
    } else {
        anyhow::bail!(
            "this xy.nc file has neither the post-2021 Cartesian x/y + LATITUDE/LONGITUDE schema \
             nor the pre-2021 lons/lats-gridded schema this endpoint supports (see the AOML TDR \
             README's 2021 gridding-change note)"
        );
    };

    let levels: Vec<f32> = ds
        .variable("level")
        .ok_or_else(|| anyhow::anyhow!("missing 'level' variable"))?
        .get_values(..)?;

    let field_var = ds.variable(var_name).ok_or_else(|| anyhow::anyhow!("missing '{var_name}' variable"))?;
    let missing = missing_value(&field_var);
    let flat: Vec<f32> = field_var.get_values(..)?;

    Ok(XyVolumeRaw { x, y, levels, flat, missing, origin_lat, origin_lon, storm_name_attr: storm_name_attr(&ds) })
}

/// Reads one field's *entire* volume (every CAPPI level, not just one) from
/// an `xy`/`xy_rel` file — backs `GET /v1/tdr/volume` for 3D rendering.
pub fn read_xy_volume(path: &Path, field: &str) -> anyhow::Result<FieldVolume> {
    let raw = read_xy_volume_raw(path, field)?;
    let data = sweep::xy_volume(&raw.flat, raw.x.len(), raw.y.len(), raw.levels.len(), raw.missing);
    Ok(FieldVolume {
        x: raw.x,
        y: raw.y,
        levels: raw.levels,
        data,
        origin_lat: raw.origin_lat,
        origin_lon: raw.origin_lon,
        storm_name_attr: raw.storm_name_attr,
    })
}

/// Max-value projection across every CAPPI level at one analysis time — the
/// "altitude" composite mode of `GET /v1/tdr/composite`.
pub fn read_xy_altitude_composite(path: &Path, field: &str) -> anyhow::Result<FieldSlice> {
    let raw = read_xy_volume_raw(path, field)?;
    let data = sweep::max_projection(&raw.flat, raw.x.len(), raw.y.len(), raw.levels.len(), raw.missing);
    Ok(FieldSlice {
        x: raw.x,
        y: raw.y,
        data,
        z_km: None,
        origin_lat: raw.origin_lat,
        origin_lon: raw.origin_lon,
        storm_name_attr: raw.storm_name_attr,
    })
}

/// Downloads + gunzips one product file into the cache dir if not already
/// there. `cache_key` should uniquely identify (mission_id, level, product,
/// analysis_time) — see `routers/tdr.rs`.
pub async fn fetch_and_cache(cache_dir: &Path, source_url: &str, cache_key: &str) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(cache_dir)?;
    let dest = cache_dir.join(format!("{cache_key}.nc"));
    if dest.exists() {
        return Ok(dest);
    }

    let client = reqwest::Client::builder()
        .user_agent("noaa-recon-api/0.1")
        .timeout(std::time::Duration::from_secs(60))
        .build()?;
    let bytes = client.get(source_url).send().await?.error_for_status()?.bytes().await?;

    let decompressed = if source_url.ends_with(".gz") {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&bytes[..]).read_to_end(&mut out)?;
        out
    } else {
        bytes.to_vec()
    };

    // Write under a unique temp name then rename (atomic on the same
    // filesystem) so a concurrent request for the same key never observes a
    // partially-written file.
    let tmp = cache_dir.join(format!("{cache_key}.{}.part", std::process::id()));
    std::fs::write(&tmp, &decompressed)?;
    std::fs::rename(&tmp, &dest)?;
    Ok(dest)
}

fn var_attr_f32(var: &netcdf::Variable, name: &str) -> Option<f32> {
    use netcdf::AttributeValue::*;
    match var.attribute_value(name)?.ok()? {
        Float(v) => Some(v),
        Double(v) => Some(v as f32),
        Int(v) => Some(v as f32),
        Short(v) => Some(v as f32),
        _ => None,
    }
}

fn missing_value(var: &netcdf::Variable) -> f32 {
    var_attr_f32(var, "missing_value").or_else(|| var_attr_f32(var, "_FillValue")).unwrap_or(DEFAULT_MISSING)
}

fn global_attr_f32(ds: &netcdf::File, name: &str) -> Option<f32> {
    use netcdf::AttributeValue::*;
    match ds.attribute(name)?.value().ok()? {
        Float(v) => Some(v),
        Double(v) => Some(v as f32),
        _ => None,
    }
}

fn global_attr_str(ds: &netcdf::File, name: &str) -> Option<String> {
    match ds.attribute(name)?.value().ok()? {
        netcdf::AttributeValue::Str(s) => Some(s),
        _ => None,
    }
}

fn storm_name_attr(ds: &netcdf::File) -> Option<String> {
    global_attr_str(ds, "STORM NAME").or_else(|| global_attr_str(ds, "STMNAME"))
}

/// Reads one field from an `xy`/`xy_rel` volume file and slices out the
/// CAPPI plane nearest `requested_z_km` (default 2.0km — above the lowest
/// level, which the AOML TDR README warns isn't representative of the
/// surface). Handles both the post-2021 Cartesian x/y schema and the
/// pre-2021 lons/lats-gridded schema (see [`read_xy_volume_raw`]).
pub fn read_xy_slice(path: &Path, field: &str, requested_z_km: Option<f32>) -> anyhow::Result<FieldSlice> {
    let raw = read_xy_volume_raw(path, field)?;
    let level_idx = sweep::nearest_index(&raw.levels, requested_z_km.unwrap_or(2.0));
    let z_km = raw.levels[level_idx];
    let data = sweep::cappi_slice(&raw.flat, raw.x.len(), raw.y.len(), raw.levels.len(), level_idx, raw.missing);

    Ok(FieldSlice {
        x: raw.x,
        y: raw.y,
        data,
        z_km: Some(z_km),
        origin_lat: raw.origin_lat,
        origin_lon: raw.origin_lon,
        storm_name_attr: raw.storm_name_attr,
    })
}

/// Reads one field from a `vert_inbound`/`vert_outbound` profile file (any
/// `_rel`/`_fall` variant) — the whole file already is the 2D (radius,
/// height) slice, no level selection needed. Errors if the file doesn't
/// have exactly one heading (this endpoint has only been verified against
/// single-azimuth profiles).
pub fn read_vert_slice(path: &Path, field: &str) -> anyhow::Result<FieldSlice> {
    let var_name = resolve_var_name(VERT_FIELD_VARS, field).ok_or_else(|| {
        anyhow::anyhow!("unknown vertical-profile field '{field}', expected one of {:?}", vert_field_names())
    })?;

    let _guard = nc_lock();
    let ds = netcdf::open(path)?;
    let heading_len = ds.dimensions().find(|d| d.name() == "heading").map(|d| d.len());
    if heading_len != Some(1) {
        anyhow::bail!("expected a single-heading vertical profile (heading dim = 1), got {heading_len:?}");
    }

    let radius: Vec<f32> = ds
        .variable("radius")
        .ok_or_else(|| anyhow::anyhow!("missing 'radius' variable"))?
        .get_values(..)?;
    let height: Vec<f32> = ds
        .variable("height")
        .ok_or_else(|| anyhow::anyhow!("missing 'height' variable"))?
        .get_values(..)?;

    let field_var = ds.variable(var_name).ok_or_else(|| anyhow::anyhow!("missing '{var_name}' variable"))?;
    let missing = missing_value(&field_var);
    let flat: Vec<f32> = field_var.get_values(..)?;

    let data = sweep::vertical_profile_slice(&flat, radius.len(), height.len(), missing);

    Ok(FieldSlice {
        x: radius,
        y: height,
        data,
        z_km: None,
        origin_lat: None,
        origin_lon: None,
        storm_name_attr: storm_name_attr(&ds),
    })
}
