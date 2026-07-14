//! GOES ABI L2 CMI fetch + decode + render — server-side port of the netCDF/S3
//! half of `app/services/goes.py`. The pure render math lives in
//! `noaa_recon_core::render`; this module does the parts that can't go to WASM:
//! S3 listing/download (reqwest) and netCDF decode (the `netcdf` C library).
//!
//! Single-band tiles (full-disk + bbox) and the composite products
//! (sandwich/geocolor) are both implemented; see `render_and_store` and
//! `render_product_and_store` respectively.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, Datelike, Duration, TimeZone, Timelike, Utc};
use futures_util::StreamExt;
use serde_json::json;

use noaa_recon_core::bbox::{bbox_bounds, bbox_out_size, BBoxRequest};
use noaa_recon_core::catalog::native_gsd_km;
use noaa_recon_core::project::{abi_to_latlon, mercator_y};
use noaa_recon_core::render::{self, Proj};

use crate::services::cache::ResultCache;
use crate::services::downloads::DownloadsRegistry;

/// HDF5 is not guaranteed thread-safe as built here; serialize every netCDF
/// open/read (port of `netcdf_lock.NC_LOCK`). Only wraps the decode, not the
/// download.
static NC_LOCK: Mutex<()> = Mutex::new(());

/// Shared netCDF/HDF5 serialization lock — recon ingest opens netCDF too, and
/// HDF5 isn't guaranteed thread-safe as built here.
pub fn nc_lock() -> std::sync::MutexGuard<'static, ()> {
    NC_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

const USER_AGENT: &str = "noaa-recon-api/0.1";

#[derive(Debug, Clone)]
pub struct ResolvedScan {
    pub bucket: String,
    pub key: String,
    pub satellite: i64,
    pub band: i64,
    pub scan_start: DateTime<Utc>,
}

/// Operational GOES-East/West satellite + bucket for a date (`_get_satellite_bucket`).
fn get_satellite_bucket(date: DateTime<Utc>, satellite: &str) -> (i64, String) {
    let ymd = (date.year(), date.month(), date.day());
    if satellite == "west" {
        if ymd >= (2023, 1, 10) {
            return (18, "noaa-goes18".into());
        }
        return (17, "noaa-goes17".into());
    }
    if ymd >= (2025, 1, 14) {
        return (19, "noaa-goes19".into());
    }
    (16, "noaa-goes16".into())
}

/// Parse the `_sYYYYDDDHHMMSSf_` scan-start stamp from an ABI key (`_parse_scan_start`).
pub fn parse_scan_start(key: &str) -> Option<DateTime<Utc>> {
    let idx = key.find("_s")?;
    let digits: &str = key.get(idx + 2..idx + 2 + 14)?;
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // Char after the 14 digits should be '_' (the file convention).
    let year: i32 = digits[0..4].parse().ok()?;
    let doy: i64 = digits[4..7].parse().ok()?;
    let hh: u32 = digits[7..9].parse().ok()?;
    let mm: u32 = digits[9..11].parse().ok()?;
    let ss: u32 = digits[11..13].parse().ok()?;
    let base = Utc.with_ymd_and_hms(year, 1, 1, hh, mm, ss).single()?;
    Some(base + Duration::days(doy - 1))
}

/// List an S3 prefix (public bucket, no auth) — `list_s3_prefix`.
async fn list_s3_prefix(bucket: &str, prefix: &str) -> anyhow::Result<Vec<String>> {
    let url = format!(
        "https://{bucket}.s3.amazonaws.com/?list-type=2&prefix={prefix}&max-keys=100"
    );
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(std::time::Duration::from_secs(20))
        .build()?;
    let xml = client.get(&url).send().await?.error_for_status()?.text().await?;

    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(&xml);
    let mut keys = Vec::new();
    let mut in_key = false;
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) if e.name().as_ref() == b"Key" => in_key = true,
            Ok(Event::End(e)) if e.name().as_ref() == b"Key" => in_key = false,
            Ok(Event::Text(t)) if in_key => keys.push(t.unescape().unwrap_or_default().into_owned()),
            Ok(Event::Eof) => break,
            Err(e) => return Err(anyhow::anyhow!("S3 XML parse: {e}")),
            _ => {}
        }
        buf.clear();
    }
    Ok(keys)
}

/// Find the ABI-L2-CMIPF scan for `band` nearest `target` (`resolve_nearest`).
/// Returns `Ok(None)` when no scan is found (-> HTTP 404 in the caller).
pub async fn resolve_nearest(
    target: DateTime<Utc>,
    band: i64,
    satellite: &str,
) -> anyhow::Result<Option<ResolvedScan>> {
    let this_hour = target
        .with_minute(0)
        .and_then(|d| d.with_second(0))
        .and_then(|d| d.with_nanosecond(0))
        .unwrap_or(target);
    let next_hour = this_hour + Duration::hours(1);
    let chan = format!("C{band:02}");

    let mut candidates: Vec<(String, String, DateTime<Utc>)> = Vec::new();
    for hour_dt in [this_hour, next_hour] {
        let (_, bucket) = get_satellite_bucket(hour_dt, satellite);
        let prefix = format!(
            "ABI-L2-CMIPF/{}/{:03}/{:02}/",
            hour_dt.year(),
            hour_dt.ordinal(),
            hour_dt.hour()
        );
        let keys = match list_s3_prefix(&bucket, &prefix).await {
            Ok(k) => k,
            Err(_) => continue,
        };
        for k in keys {
            if !k.contains(&chan) || !k.ends_with(".nc") {
                continue;
            }
            if let Some(ss) = parse_scan_start(&k) {
                candidates.push((bucket.clone(), k, ss));
            }
        }
    }

    let best = candidates.into_iter().min_by_key(|c| (c.2 - target).num_seconds().abs());
    Ok(best.map(|(bucket, key, scan_start)| {
        let (sat_num, _) = get_satellite_bucket(scan_start, satellite);
        ResolvedScan { bucket, key, satellite: sat_num, band, scan_start }
    }))
}

/// Given an already-resolved scan, finds the sibling file for a different
/// band from the exact same scan cycle (`resolve_companion_band`) — every
/// ABI band is captured simultaneously per scan, so the sibling file shares
/// `resolved`'s exact `scan_start` (not just "close"), which is what lets
/// the composite products (sandwich, geocolor) combine multiple bands
/// without any time-alignment error between them.
async fn resolve_companion_band(resolved: &ResolvedScan, band: i64) -> anyhow::Result<ResolvedScan> {
    let hour_dt = resolved
        .scan_start
        .with_minute(0)
        .and_then(|d| d.with_second(0))
        .and_then(|d| d.with_nanosecond(0))
        .unwrap_or(resolved.scan_start);
    let prefix = format!(
        "ABI-L2-CMIPF/{}/{:03}/{:02}/",
        hour_dt.year(),
        hour_dt.ordinal(),
        hour_dt.hour()
    );
    let chan = format!("C{band:02}");
    let keys = list_s3_prefix(&resolved.bucket, &prefix).await?;
    for k in keys {
        if !k.contains(&chan) || !k.ends_with(".nc") {
            continue;
        }
        if parse_scan_start(&k) == Some(resolved.scan_start) {
            return Ok(ResolvedScan {
                bucket: resolved.bucket.clone(),
                key: k,
                satellite: resolved.satellite,
                band,
                scan_start: resolved.scan_start,
            });
        }
    }
    anyhow::bail!(
        "No companion Band {band} file found for the scan at {} in {} (needed to render this composite product)",
        resolved.scan_start.to_rfc3339(),
        resolved.bucket
    )
}

/// Live per-render-job download progress, surfaced to the console through the
/// cache's lock file (`ResultCache::update_progress`) so `/status/{key}`
/// reports live per-band byte counts instead of just an opaque "generating"
/// while a composite's several companion-band downloads are in flight.
/// Writes are throttled (`FLUSH_INTERVAL`) so a fast connection doesn't turn
/// every network chunk into a disk write.
struct ProgressTracker {
    cache: ResultCache,
    key: String,
    bands: Mutex<std::collections::BTreeMap<i64, (u64, u64)>>,
    last_flush: Mutex<std::time::Instant>,
}

impl ProgressTracker {
    const FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(300);

    fn new(cache: ResultCache, key: String) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            cache,
            key,
            bands: Mutex::new(Default::default()),
            last_flush: Mutex::new(std::time::Instant::now()),
        })
    }

    /// `force`: bypass the flush throttle — used for a download's first and
    /// last report so "0 bytes" and "done" are never dropped by unlucky timing.
    fn report(&self, band: i64, downloaded: u64, total: u64, force: bool) {
        self.bands.lock().unwrap().insert(band, (downloaded, total));
        if !force {
            let mut last = self.last_flush.lock().unwrap();
            if last.elapsed() < Self::FLUSH_INTERVAL {
                return;
            }
            *last = std::time::Instant::now();
        }
        let downloads: Vec<serde_json::Value> = self
            .bands
            .lock()
            .unwrap()
            .iter()
            .map(|(band, (downloaded, total))| json!({ "band": band, "bytes": downloaded, "total_bytes": total }))
            .collect();
        self.cache.update_progress(&self.key, &json!({ "downloads": downloads }));
    }
}

/// Download the object to `nc_cache_dir` if not already cached
/// (`ensure_downloaded`). Streams to a temp file (renamed into place once
/// complete, so a crash mid-download never leaves a corrupt cached `.nc`),
/// reporting progress through `progress` if given.
async fn ensure_downloaded(
    resolved: &ResolvedScan,
    nc_cache_dir: &Path,
    downloads: &DownloadsRegistry,
    progress: Option<&std::sync::Arc<ProgressTracker>>,
) -> anyhow::Result<PathBuf> {
    tokio::fs::create_dir_all(nc_cache_dir).await?;
    let filename = resolved.key.rsplit('/').next().unwrap_or("scan.nc");
    let local_path = nc_cache_dir.join(filename);
    if local_path.exists() {
        if let Some(p) = progress {
            let size = tokio::fs::metadata(&local_path).await.map(|m| m.len()).unwrap_or(0);
            p.report(resolved.band, size, size, true);
        }
        return Ok(local_path);
    }
    let url = format!("https://{}.s3.amazonaws.com/{}", resolved.bucket, resolved.key);
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(std::time::Duration::from_secs(180))
        .build()?;
    let resp = client.get(&url).send().await?.error_for_status()?;
    let total = resp.content_length().unwrap_or(0);
    if let Some(p) = progress {
        p.report(resolved.band, 0, total, true);
    }
    downloads.start(filename, total);

    let tmp_path = nc_cache_dir.join(format!("{filename}.part"));
    // Run the actual streaming in a block so `downloads.finish` runs on every
    // exit path (success or error via `?`) — otherwise a failed/aborted
    // download would leave a stale "in progress" entry in the registry
    // forever, since there's no `finally` to hang cleanup off of.
    let result: anyhow::Result<u64> = async {
        let mut file = tokio::fs::File::create(&tmp_path).await?;
        let mut downloaded: u64 = 0;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await?;
            downloaded += chunk.len() as u64;
            downloads.update(filename, downloaded);
            if let Some(p) = progress {
                p.report(resolved.band, downloaded, total, false);
            }
        }
        tokio::io::AsyncWriteExt::flush(&mut file).await?;
        drop(file);
        tokio::fs::rename(&tmp_path, &local_path).await?;
        Ok(downloaded)
    }
    .await;
    downloads.finish(filename);
    let downloaded = result?;

    if let Some(p) = progress {
        p.report(resolved.band, downloaded, downloaded.max(total), true);
    }
    Ok(local_path)
}

// ── netCDF decode ────────────────────────────────────────────────────────────

struct Source {
    cmi: Vec<f32>,
    x: Vec<f64>,
    y: Vec<f64>,
    proj: Proj,
}

fn attr_f64(var: &netcdf::Variable, name: &str) -> Option<f64> {
    use netcdf::AttributeValue::*;
    match var.attribute_value(name)?.ok()? {
        Uchar(v) => Some(v as f64),
        Schar(v) => Some(v as f64),
        Ushort(v) => Some(v as f64),
        Short(v) => Some(v as f64),
        Uint(v) => Some(v as f64),
        Int(v) => Some(v as f64),
        Ulonglong(v) => Some(v as f64),
        Longlong(v) => Some(v as f64),
        Float(v) => Some(v as f64),
        Double(v) => Some(v),
        _ => None,
    }
}

fn attr_pair_f64(var: &netcdf::Variable, name: &str) -> Option<(f64, f64)> {
    use netcdf::AttributeValue::*;
    let two = |a: f64, b: f64| Some((a, b));
    match var.attribute_value(name)?.ok()? {
        Shorts(v) if v.len() == 2 => two(v[0] as f64, v[1] as f64),
        Ushorts(v) if v.len() == 2 => two(v[0] as f64, v[1] as f64),
        Ints(v) if v.len() == 2 => two(v[0] as f64, v[1] as f64),
        Floats(v) if v.len() == 2 => two(v[0] as f64, v[1] as f64),
        Doubles(v) if v.len() == 2 => two(v[0], v[1]),
        _ => None,
    }
}

/// Apply _FillValue/valid_range masking then scale_factor/add_offset — the
/// equivalent of netCDF4-python's default mask-and-scale, which the Rust crate
/// doesn't do automatically.
fn scale_and_mask(raw: &[f32], scale: f64, offset: f64, fill: Option<f64>, valid: Option<(f64, f64)>) -> Vec<f32> {
    raw.iter()
        .map(|&r| {
            let r = r as f64;
            if let Some(f) = fill {
                if r == f {
                    return f32::NAN;
                }
            }
            if let Some((lo, hi)) = valid {
                if r < lo || r > hi {
                    return f32::NAN;
                }
            }
            (r * scale + offset) as f32
        })
        .collect()
}

fn read_proj(ds: &netcdf::File) -> anyhow::Result<Proj> {
    let proj = ds
        .variable("goes_imager_projection")
        .ok_or_else(|| anyhow::anyhow!("missing goes_imager_projection"))?;
    Ok(Proj {
        sat_lon: attr_f64(&proj, "longitude_of_projection_origin").ok_or_else(|| anyhow::anyhow!("no sat_lon"))?,
        h: attr_f64(&proj, "perspective_point_height").ok_or_else(|| anyhow::anyhow!("no height"))?,
        r_eq: attr_f64(&proj, "semi_major_axis").ok_or_else(|| anyhow::anyhow!("no r_eq"))?,
        r_pol: attr_f64(&proj, "semi_minor_axis").ok_or_else(|| anyhow::anyhow!("no r_pol"))?,
    })
}

fn cmi_scale(var: &netcdf::Variable) -> (f64, f64, Option<f64>, Option<(f64, f64)>) {
    (
        attr_f64(var, "scale_factor").unwrap_or(1.0),
        attr_f64(var, "add_offset").unwrap_or(0.0),
        attr_f64(var, "_FillValue"),
        attr_pair_f64(var, "valid_range"),
    )
}

/// Read a 1-D coordinate variable (`x`/`y`) applying its scale_factor/add_offset.
/// GOES stores x/y as scaled shorts (radians only after scaling) — the same
/// mask-and-scale netCDF4-python applies automatically, which we must replicate.
fn read_coord<E: Into<netcdf::Extents>>(
    ds: &netcdf::File,
    name: &str,
    extents: E,
) -> anyhow::Result<Vec<f64>> {
    let var = ds.variable(name).ok_or_else(|| anyhow::anyhow!("missing coord {name}"))?;
    let scale = attr_f64(&var, "scale_factor").unwrap_or(1.0);
    let offset = attr_f64(&var, "add_offset").unwrap_or(0.0);
    let raw: Vec<f64> = var.get_values::<f64, _>(extents)?;
    Ok(raw.into_iter().map(|v| v * scale + offset).collect())
}

/// Strided full-disk read (`_read_source_downsampled`).
fn read_source_downsampled(nc_path: &Path, step: Option<usize>) -> anyhow::Result<Source> {
    let _guard = NC_LOCK.lock().unwrap();
    let ds = netcdf::open(nc_path)?;
    let cmi_var = ds.variable("CMI").ok_or_else(|| anyhow::anyhow!("no CMI variable"))?;
    let dims = cmi_var.dimensions();
    let (ny, nx) = (dims[0].len(), dims[1].len());
    let step = step.unwrap_or_else(|| (ny.max(nx) / 2160).max(1));

    let ext = |n: usize| netcdf::Extent::from((0..n).step_by(step));
    let raw: Vec<f32> = cmi_var.get_values::<f32, _>([ext(ny), ext(nx)])?;
    let (scale, offset, fill, valid) = cmi_scale(&cmi_var);
    let cmi = scale_and_mask(&raw, scale, offset, fill, valid);

    let x = read_coord(&ds, "x", (0..nx).step_by(step))?;
    let y = read_coord(&ds, "y", (0..ny).step_by(step))?;
    let proj = read_proj(&ds)?;
    anyhow::ensure!(cmi.len() == x.len() * y.len(), "CMI/coord shape mismatch");
    Ok(Source { cmi, x, y, proj })
}

/// Locate-then-crop native read (`_read_source_cropped`). Returns `Ok(None)` if
/// the box misses this scan's disk.
fn read_source_cropped(
    nc_path: &Path,
    lat_s: f64,
    lat_n: f64,
    lon_w: f64,
    lon_e: f64,
    fine_step: usize,
) -> anyhow::Result<Option<Source>> {
    const LOCATE_GRID: usize = 160;
    let _guard = NC_LOCK.lock().unwrap();
    let ds = netcdf::open(nc_path)?;
    let cmi_var = ds.variable("CMI").ok_or_else(|| anyhow::anyhow!("no CMI variable"))?;
    let dims = cmi_var.dimensions();
    let (ny, nx) = (dims[0].len(), dims[1].len());
    let x_full = read_coord(&ds, "x", 0..nx)?;
    let y_full = read_coord(&ds, "y", 0..ny)?;
    let proj = read_proj(&ds)?;

    let step_y = (ny / LOCATE_GRID).max(1);
    let step_x = (nx / LOCATE_GRID).max(1);
    // Sparse locate pass over the 1-D coords only.
    let (mut r_lo, mut r_hi, mut c_lo, mut c_hi) = (usize::MAX, 0usize, usize::MAX, 0usize);
    let mut hit = false;
    let mut si = 0;
    while si < ny {
        let mut sj = 0;
        while sj < nx {
            if let Some((lon, lat)) = abi_to_latlon(x_full[sj], y_full[si], proj.sat_lon, proj.h, proj.r_eq, proj.r_pol) {
                if lon >= lon_w && lon <= lon_e && lat >= lat_s && lat <= lat_n {
                    hit = true;
                    r_lo = r_lo.min(si);
                    r_hi = r_hi.max(si);
                    c_lo = c_lo.min(sj);
                    c_hi = c_hi.max(sj);
                }
            }
            sj += step_x;
        }
        si += step_y;
    }
    if !hit {
        return Ok(None);
    }
    let pad_y = step_y * 2;
    let pad_x = step_x * 2;
    let row_lo = r_lo.saturating_sub(pad_y);
    let row_hi = ((r_hi + 1) + pad_y).min(ny);
    let col_lo = c_lo.saturating_sub(pad_x);
    let col_hi = ((c_hi + 1) + pad_x).min(nx);

    // Read x/y crop through the SAME netcdf striding as CMI so element counts
    // always agree (mixing Rust's .step_by with the crate's produced off-by-a-few
    // mismatches at the tail).
    let x = read_coord(&ds, "x", (col_lo..col_hi).step_by(fine_step))?;
    let y = read_coord(&ds, "y", (row_lo..row_hi).step_by(fine_step))?;
    let raw: Vec<f32> = cmi_var.get_values::<f32, _>([
        netcdf::Extent::from((row_lo..row_hi).step_by(fine_step)),
        netcdf::Extent::from((col_lo..col_hi).step_by(fine_step)),
    ])?;
    let (scale, offset, fill, valid) = cmi_scale(&cmi_var);
    let cmi = scale_and_mask(&raw, scale, offset, fill, valid);
    anyhow::ensure!(cmi.len() == x.len() * y.len(), "crop CMI/coord shape mismatch");
    Ok(Some(Source { cmi, x, y, proj }))
}

/// Delete cached raw GOES netCDF files older than `max_age_hours` from
/// `nc_cache_dir` — port of `scripts/clear_nc_cache.py`. Safe any time (files are
/// re-downloaded on demand). Returns (files_removed, bytes_freed).
pub fn clean_nc_cache(nc_cache_dir: &Path, max_age_hours: f64) -> (usize, u64) {
    let cutoff = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs_f64(max_age_hours * 3600.0))
        .unwrap_or(std::time::UNIX_EPOCH);
    let (mut removed, mut freed) = (0usize, 0u64);
    if let Ok(entries) = std::fs::read_dir(nc_cache_dir) {
        for e in entries.flatten() {
            let md = match e.metadata() {
                Ok(m) if m.is_file() => m,
                _ => continue,
            };
            let old = md.modified().map(|m| m < cutoff).unwrap_or(false);
            if old {
                freed += md.len();
                if std::fs::remove_file(e.path()).is_ok() {
                    removed += 1;
                }
            }
        }
    }
    (removed, freed)
}

/// Structural metadata for a netCDF file (dimensions, variables, global attrs) —
/// the console's `ncdump -h`-style preview (`get_goes_nc_info`).
pub fn nc_info(path: &Path) -> anyhow::Result<serde_json::Value> {
    let _guard = NC_LOCK.lock().unwrap();
    let ds = netcdf::open(path)?;
    let dimensions: serde_json::Map<String, serde_json::Value> = ds
        .dimensions()
        .map(|d| (d.name(), json!(d.len())))
        .collect();
    let mut variables: Vec<serde_json::Value> = ds
        .variables()
        .map(|v| {
            let dims: Vec<String> = v.dimensions().iter().map(|d| d.name()).collect();
            let shape: Vec<usize> = v.dimensions().iter().map(|d| d.len()).collect();
            json!({
                "name": v.name(),
                "dimensions": dims,
                "shape": shape,
                "dtype": format!("{:?}", v.vartype()),
                "units": attr_str(&v, "units"),
                "long_name": attr_str(&v, "long_name"),
            })
        })
        .collect();
    variables.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    let global_attrs: serde_json::Map<String, serde_json::Value> = ds
        .attributes()
        .filter_map(|a| a.value().ok().map(|v| (a.name().to_string(), json!(format!("{v:?}")))))
        .collect();
    Ok(json!({
        "dimensions": dimensions,
        "variables": variables,
        "global_attrs": global_attrs,
    }))
}

fn attr_str(var: &netcdf::Variable, name: &str) -> Option<String> {
    match var.attribute_value(name)?.ok()? {
        netcdf::AttributeValue::Str(s) => Some(s),
        other => Some(format!("{other:?}")),
    }
}

// ── Orchestration ────────────────────────────────────────────────────────────

/// Background-task entry point (`render_and_store`): download, decode, render,
/// PNG-encode, and write the result into the shared cache. Never panics — all
/// failures are reported to the client via an `error` cache entry.
pub async fn render_and_store(
    resolved: ResolvedScan,
    cmap: String,
    key: String,
    nc_cache_dir: PathBuf,
    cache: ResultCache,
    bbox: Option<BBoxRequest>,
    downloads: std::sync::Arc<DownloadsRegistry>,
) {
    let result =
        render_and_store_inner(&resolved, &cmap, &key, &nc_cache_dir, &cache, bbox.as_ref(), &downloads).await;
    if let Err(e) = result {
        tracing::error!("GOES render failed for key={key}: {e:#}");
        let mut meta = json!({
            "status": "error",
            "key": key,
            "message": e.to_string(),
            "band": resolved.band,
            "cmap": cmap,
            "satellite": format!("GOES-{}", resolved.satellite),
            "scan_start": resolved.scan_start.to_rfc3339(),
        });
        if let Some(b) = bbox {
            let m = meta.as_object_mut().unwrap();
            m.insert("center".into(), json!([b.center_lat, b.center_lon]));
            m.insert("width_km".into(), json!(b.width_km));
        }
        let _ = cache.write_result(&key, &meta);
    }
}

async fn render_and_store_inner(
    resolved: &ResolvedScan,
    cmap: &str,
    key: &str,
    nc_cache_dir: &Path,
    cache: &ResultCache,
    bbox: Option<&BBoxRequest>,
    downloads: &DownloadsRegistry,
) -> anyhow::Result<()> {
    let progress = ProgressTracker::new(cache.clone(), key.to_string());
    let nc_path = ensure_downloaded(resolved, nc_cache_dir, downloads, Some(&progress)).await?;
    let out_png = cache.output_path(key, "png");
    let band = resolved.band;
    let cmap_s = cmap.to_string();
    let bbox = bbox.copied();

    // netCDF decode + render + PNG encode is blocking/CPU-bound.
    let render_meta = tokio::task::spawn_blocking(move || -> anyhow::Result<serde_json::Value> {
        let (rgba, out_size, bounds, sat_lon, extra) = match bbox {
            None => {
                let src = read_source_downsampled(&nc_path, None)?;
                let r = render::render_full_disk(
                    &src.cmi, src.y.len(), src.x.len(), &src.x, &src.y, src.proj, &cmap_s, band, 2048,
                );
                (r.rgba, r.out_size, r.bounds, r.sat_lon, None)
            }
            Some(b) => {
                let (lat_s, lat_n, lon_w, lon_e) = bbox_bounds(b.center_lat, b.center_lon, b.width_km);
                let gsd = native_gsd_km(band).unwrap_or(2.0);
                let fine_step = ((b.resolution_km / gsd).round() as usize).max(1);
                let src = read_source_cropped(&nc_path, lat_s, lat_n, lon_w, lon_e, fine_step)?
                    .ok_or_else(|| anyhow::anyhow!(
                        "Requested area ({lat_s:.2},{lon_w:.2})-({lat_n:.2},{lon_e:.2}) is outside this scan's visible disk"
                    ))?;
                let out_size = bbox_out_size(b.width_km, b.resolution_km);
                let r = render::render_bbox(
                    &src.cmi, src.y.len(), src.x.len(), &src.x, &src.y, src.proj, &cmap_s, band,
                    b.center_lat, b.center_lon, b.width_km, out_size,
                )
                .map_err(|e| anyhow::anyhow!(e))?;
                (r.rgba, r.out_size, r.bounds, r.sat_lon, Some(b.resolution_km))
            }
        };
        // Encode RGBA -> PNG (Pillow equivalent).
        let img = image::RgbaImage::from_raw(out_size as u32, out_size as u32, rgba)
            .ok_or_else(|| anyhow::anyhow!("RGBA buffer size mismatch"))?;
        if let Some(parent) = out_png.parent() {
            std::fs::create_dir_all(parent)?;
        }
        img.save_with_format(&out_png, image::ImageFormat::Png)?;
        Ok(json!({ "bounds": bounds, "sat_lon": sat_lon, "resolution_km": extra }))
    })
    .await??;

    let mut meta = json!({
        "status": "ready",
        "key": key,
        "png_url": format!("/cache/satellite/{key}.png"),
        "bounds": render_meta["bounds"],
        "band": resolved.band,
        "cmap": cmap,
        "satellite": format!("GOES-{}", resolved.satellite),
        "sat_lon": render_meta["sat_lon"],
        "scan_start": resolved.scan_start.to_rfc3339(),
    });
    if let Some(b) = bbox {
        let m = meta.as_object_mut().unwrap();
        m.insert("center".into(), json!([b.center_lat, b.center_lon]));
        m.insert("width_km".into(), json!(b.width_km));
        m.insert("resolution_km".into(), render_meta["resolution_km"].clone());
    }
    cache.write_result(key, &meta)?;
    Ok(())
}

// ── Composite products (sandwich / geocolor) ────────────────────────────────

/// Background-task entry point for a composite product, the `product=`
/// counterpart of `render_and_store`: resolves + downloads every companion
/// band the product needs (`resolved_ir` is Band 13's already-resolved scan;
/// every other band is its companion — see `resolve_companion_band`), then
/// reprojects each band onto one shared canvas and composes/colors them
/// (`noaa_recon_core::render::render_sandwich`/`render_geocolor`). Never
/// panics — all failures are reported to the client via an `error` cache
/// entry, same as `render_and_store`.
pub async fn render_product_and_store(
    product: String,
    resolved_ir: ResolvedScan,
    key: String,
    nc_cache_dir: PathBuf,
    cache: ResultCache,
    bbox: Option<BBoxRequest>,
    downloads: std::sync::Arc<DownloadsRegistry>,
) {
    let result = render_product_and_store_inner(
        &product,
        &resolved_ir,
        &key,
        &nc_cache_dir,
        &cache,
        bbox.as_ref(),
        &downloads,
    )
    .await;
    if let Err(e) = result {
        tracing::error!("Composite render failed for key={key} product={product}: {e:#}");
        let mut meta = json!({
            "status": "error",
            "key": key,
            "message": e.to_string(),
            "product": product,
            "satellite": format!("GOES-{}", resolved_ir.satellite),
            "scan_start": resolved_ir.scan_start.to_rfc3339(),
        });
        if let Some(b) = bbox {
            let m = meta.as_object_mut().unwrap();
            m.insert("center".into(), json!([b.center_lat, b.center_lon]));
            m.insert("width_km".into(), json!(b.width_km));
        }
        let _ = cache.write_result(&key, &meta);
    }
}

async fn render_product_and_store_inner(
    product: &str,
    resolved_ir: &ResolvedScan,
    key: &str,
    nc_cache_dir: &Path,
    cache: &ResultCache,
    bbox: Option<&BBoxRequest>,
    downloads: &DownloadsRegistry,
) -> anyhow::Result<()> {
    let band_list: &[i64] = match product {
        "sandwich" => &[13, 2],
        "geocolor" => &[1, 2, 3, 13],
        _ => anyhow::bail!("unknown composite product: {product}"),
    };

    // Resolve every band this product needs (band 13 is `resolved_ir` itself
    // — every other band is its companion, each an independent S3 listing
    // call) concurrently rather than one at a time — geocolor's 4 bands
    // otherwise pay 4x the listing latency for no reason, since none of
    // these calls depend on each other.
    let resolved_list: Vec<(i64, ResolvedScan)> = futures_util::future::try_join_all(band_list.iter().map(|&band| {
        let resolved_ir = resolved_ir.clone();
        async move {
            let resolved = if band == resolved_ir.band {
                resolved_ir
            } else {
                resolve_companion_band(&resolved_ir, band).await?
            };
            Ok::<_, anyhow::Error>((band, resolved))
        }
    }))
    .await?;

    // Likewise download every band's file concurrently — this is the real
    // win for a composite (geocolor fetches ~4x a single band's ~25MB at
    // once instead of serially), bounded only by the S3 connection/bandwidth
    // available, not by however many companion bands the product needs.
    let progress = ProgressTracker::new(cache.clone(), key.to_string());
    let downloaded: Vec<(i64, PathBuf)> = futures_util::future::try_join_all(resolved_list.iter().map(|(band, resolved)| {
        let progress = progress.clone();
        async move {
            let path = ensure_downloaded(resolved, nc_cache_dir, downloads, Some(&progress)).await?;
            Ok::<_, anyhow::Error>((*band, path))
        }
    }))
    .await?;
    let nc_paths: std::collections::HashMap<i64, PathBuf> = downloaded.into_iter().collect();

    let out_png = cache.output_path(key, "png");
    let product_s = product.to_string();
    let bbox_c = bbox.copied();
    let scan_start = resolved_ir.scan_start;

    let render_meta = tokio::task::spawn_blocking(move || -> anyhow::Result<serde_json::Value> {
        let (rgba, out_size, bounds, sat_lon, extra) = match product_s.as_str() {
            "sandwich" => render_sandwich_product(&nc_paths[&13], &nc_paths[&2], bbox_c)?,
            "geocolor" => render_geocolor_product(
                &nc_paths[&1],
                &nc_paths[&2],
                &nc_paths[&3],
                &nc_paths[&13],
                scan_start,
                bbox_c,
            )?,
            other => anyhow::bail!("unknown composite product: {other}"),
        };
        let img = image::RgbaImage::from_raw(out_size as u32, out_size as u32, rgba)
            .ok_or_else(|| anyhow::anyhow!("RGBA buffer size mismatch"))?;
        if let Some(parent) = out_png.parent() {
            std::fs::create_dir_all(parent)?;
        }
        img.save_with_format(&out_png, image::ImageFormat::Png)?;
        Ok(json!({ "bounds": bounds, "sat_lon": sat_lon, "resolution_km": extra }))
    })
    .await??;

    let mut meta = json!({
        "status": "ready",
        "key": key,
        "png_url": format!("/cache/satellite/{key}.png"),
        "bounds": render_meta["bounds"],
        "product": product,
        "satellite": format!("GOES-{}", resolved_ir.satellite),
        "sat_lon": render_meta["sat_lon"],
        "scan_start": resolved_ir.scan_start.to_rfc3339(),
    });
    if let Some(b) = bbox {
        let m = meta.as_object_mut().unwrap();
        m.insert("center".into(), json!([b.center_lat, b.center_lon]));
        m.insert("width_km".into(), json!(b.width_km));
        m.insert("resolution_km".into(), render_meta["resolution_km"].clone());
    }
    cache.write_result(key, &meta)?;
    Ok(())
}

type RenderOutcome = (Vec<u8>, usize, [[f64; 2]; 2], f64, Option<f64>);

/// Full-disk canvas bounds shared by every band of a scan (same constants
/// `render_full_disk` uses for a single band).
const FULL_DISK_LAT: (f64, f64) = (-81.3, 81.3);

fn render_sandwich_product(
    nc_ir: &Path,
    nc_vis: &Path,
    bbox: Option<BBoxRequest>,
) -> anyhow::Result<RenderOutcome> {
    match bbox {
        None => {
            let ir_src = read_source_downsampled(nc_ir, None)?;
            let vis_src = read_source_downsampled(nc_vis, None)?;
            let (lat_s, lat_n) = FULL_DISK_LAT;
            let (lon_w, lon_e) = (ir_src.proj.sat_lon - 81.0, ir_src.proj.sat_lon + 81.0);
            let (merc_y_s, merc_y_n) = (mercator_y(lat_s), mercator_y(lat_n));
            let out_size = 2048usize;

            let ir_canvas = render::project_band_to_canvas(
                &ir_src.cmi, ir_src.y.len(), ir_src.x.len(), &ir_src.x, &ir_src.y, ir_src.proj,
                lon_w, lon_e, merc_y_s, merc_y_n, out_size,
            )
            .unwrap_or_else(|| vec![f32::NAN; out_size * out_size]);
            let vis_canvas = render::project_band_to_canvas(
                &vis_src.cmi, vis_src.y.len(), vis_src.x.len(), &vis_src.x, &vis_src.y, ir_src.proj,
                lon_w, lon_e, merc_y_s, merc_y_n, out_size,
            );

            let r = render::render_sandwich(
                &ir_canvas, vis_canvas.as_deref(), out_size, [[lat_s, lon_w], [lat_n, lon_e]], ir_src.proj.sat_lon,
            );
            Ok((r.rgba, r.out_size, r.bounds, r.sat_lon, None))
        }
        Some(b) => {
            let (lat_s, lat_n, lon_w, lon_e) = bbox_bounds(b.center_lat, b.center_lon, b.width_km);
            let (merc_y_s, merc_y_n) = (mercator_y(lat_s), mercator_y(lat_n));
            let out_size = bbox_out_size(b.width_km, b.resolution_km);

            let ir_step = fine_step(b.resolution_km, 13);
            let ir_src = read_source_cropped(nc_ir, lat_s, lat_n, lon_w, lon_e, ir_step)?.ok_or_else(|| {
                anyhow::anyhow!(
                    "Requested area ({lat_s:.2},{lon_w:.2})-({lat_n:.2},{lon_e:.2}) is outside this scan's visible disk"
                )
            })?;
            let ir_canvas = render::project_band_to_canvas(
                &ir_src.cmi, ir_src.y.len(), ir_src.x.len(), &ir_src.x, &ir_src.y, ir_src.proj,
                lon_w, lon_e, merc_y_s, merc_y_n, out_size,
            )
            .ok_or_else(|| anyhow::anyhow!("Requested area has no valid data in this scan (off-disk or no-data)"))?;

            let vis_step = fine_step(b.resolution_km, 2);
            let vis_canvas = read_source_cropped(nc_vis, lat_s, lat_n, lon_w, lon_e, vis_step)?.and_then(|src| {
                render::project_band_to_canvas(
                    &src.cmi, src.y.len(), src.x.len(), &src.x, &src.y, ir_src.proj,
                    lon_w, lon_e, merc_y_s, merc_y_n, out_size,
                )
            });

            let r = render::render_sandwich(
                &ir_canvas, vis_canvas.as_deref(), out_size, [[lat_s, lon_w], [lat_n, lon_e]], ir_src.proj.sat_lon,
            );
            Ok((r.rgba, r.out_size, r.bounds, r.sat_lon, Some(b.resolution_km)))
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_geocolor_product(
    nc_c1: &Path,
    nc_c2: &Path,
    nc_c3: &Path,
    nc_ir: &Path,
    scan_time: DateTime<Utc>,
    bbox: Option<BBoxRequest>,
) -> anyhow::Result<RenderOutcome> {
    let doy = scan_time.ordinal() as i64;
    let (hour, minute, second) = (scan_time.hour(), scan_time.minute(), scan_time.second() as f64);

    match bbox {
        None => {
            let ir_src = read_source_downsampled(nc_ir, None)?;
            let c1_src = read_source_downsampled(nc_c1, None)?;
            let c2_src = read_source_downsampled(nc_c2, None)?;
            let c3_src = read_source_downsampled(nc_c3, None)?;
            let (lat_s, lat_n) = FULL_DISK_LAT;
            let (lon_w, lon_e) = (ir_src.proj.sat_lon - 81.0, ir_src.proj.sat_lon + 81.0);
            let (merc_y_s, merc_y_n) = (mercator_y(lat_s), mercator_y(lat_n));
            let out_size = 2048usize;

            let proj_band = |src: &Source| {
                render::project_band_to_canvas(
                    &src.cmi, src.y.len(), src.x.len(), &src.x, &src.y, ir_src.proj,
                    lon_w, lon_e, merc_y_s, merc_y_n, out_size,
                )
            };
            let ir_canvas = proj_band(&ir_src).unwrap_or_else(|| vec![f32::NAN; out_size * out_size]);
            let blue = proj_band(&c1_src);
            let red = proj_band(&c2_src);
            let veggie = proj_band(&c3_src);

            let r = render::render_geocolor(
                &ir_canvas, blue.as_deref(), red.as_deref(), veggie.as_deref(), out_size,
                [[lat_s, lon_w], [lat_n, lon_e]], lon_w, lon_e, merc_y_s, merc_y_n, ir_src.proj.sat_lon,
                doy, hour, minute, second,
            );
            Ok((r.rgba, r.out_size, r.bounds, r.sat_lon, None))
        }
        Some(b) => {
            let (lat_s, lat_n, lon_w, lon_e) = bbox_bounds(b.center_lat, b.center_lon, b.width_km);
            let (merc_y_s, merc_y_n) = (mercator_y(lat_s), mercator_y(lat_n));
            let out_size = bbox_out_size(b.width_km, b.resolution_km);

            let ir_step = fine_step(b.resolution_km, 13);
            let ir_src = read_source_cropped(nc_ir, lat_s, lat_n, lon_w, lon_e, ir_step)?.ok_or_else(|| {
                anyhow::anyhow!(
                    "Requested area ({lat_s:.2},{lon_w:.2})-({lat_n:.2},{lon_e:.2}) is outside this scan's visible disk"
                )
            })?;
            let ir_canvas = render::project_band_to_canvas(
                &ir_src.cmi, ir_src.y.len(), ir_src.x.len(), &ir_src.x, &ir_src.y, ir_src.proj,
                lon_w, lon_e, merc_y_s, merc_y_n, out_size,
            )
            .ok_or_else(|| anyhow::anyhow!("Requested area has no valid data in this scan (off-disk or no-data)"))?;

            let load = |path: &Path, band: i64| -> anyhow::Result<Option<Vec<f32>>> {
                let step = fine_step(b.resolution_km, band);
                Ok(read_source_cropped(path, lat_s, lat_n, lon_w, lon_e, step)?.and_then(|src| {
                    render::project_band_to_canvas(
                        &src.cmi, src.y.len(), src.x.len(), &src.x, &src.y, ir_src.proj,
                        lon_w, lon_e, merc_y_s, merc_y_n, out_size,
                    )
                }))
            };
            let blue = load(nc_c1, 1)?;
            let red = load(nc_c2, 2)?;
            let veggie = load(nc_c3, 3)?;

            let r = render::render_geocolor(
                &ir_canvas, blue.as_deref(), red.as_deref(), veggie.as_deref(), out_size,
                [[lat_s, lon_w], [lat_n, lon_e]], lon_w, lon_e, merc_y_s, merc_y_n, ir_src.proj.sat_lon,
                doy, hour, minute, second,
            );
            Ok((r.rgba, r.out_size, r.bounds, r.sat_lon, Some(b.resolution_km)))
        }
    }
}

/// `max(1, round(resolution_km / band's native GSD))` — the bbox crop's
/// per-band netCDF read stride (same formula `render_and_store_inner` uses
/// for a single band).
fn fine_step(resolution_km: f64, band: i64) -> usize {
    let gsd = native_gsd_km(band).unwrap_or(2.0);
    ((resolution_km / gsd).round() as usize).max(1)
}
