//! Recon MET ingest — port of the crawler/harvester/reconciler half of
//! `app/services/recon_met.py`. Crawls NOAA's unlabeled recon archive
//! (seb.omao.noaa.gov), downloads each mission's best-QC netCDF-3 file, decimates
//! its flight-level observations, resolves the storm name from PDF/netCDF
//! metadata, and then runs the reconciliation layer that figures out which storm
//! each mission actually flew (by haversine-matching flight tracks against the
//! storms DB).
//!
//! The read path lives in `recon_met.rs`; this is ingest only.

use std::io::Write;
use std::path::Path;

use chrono::{DateTime, Datelike, Utc};
use rusqlite::Connection;
use serde_json::{json, Map, Value};

use crate::services::recon_met;
use crate::services::storms;

const BASE_URL: &str = "https://seb.omao.noaa.gov/pub/acdata";
const DECIMATION: usize = 5; // store every Nth 1-second obs (5 -> 0.2 Hz)
const HTTP_TIMEOUT_SECS: u64 = 30;
const NC_TIMEOUT_SECS: u64 = 120;
const MS_TO_KT: f64 = 1.94384;

const TRAINING_BUCKET_NAME: &str = "Training / Research";
const LEGACY_TRAINING_BUCKET_NAMES: [&str; 2] = ["Unknown / Training", "Training Flights / Research"];
const MANUAL_STORM_NAME_CORRECTIONS: [(&str, &str); 1] = [("20260616H1", TRAINING_BUCKET_NAME)];

const MAX_STORM_MATCH_DISTANCE_KM: f64 = 500.0;
const MAX_STORM_MATCH_TIME_HOURS: f64 = 30.0;

fn junk_storm_names() -> Vec<String> {
    let mut v: Vec<String> = ["CYCLONE", "TDR", "SURV", "SURVEY", "RECON", "INVEST"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    v.push(TRAINING_BUCKET_NAME.to_uppercase());
    v
}

fn tailnum_name(raw: &str) -> String {
    match raw {
        "n42" => "NOAA 42 (Kermit)".into(),
        "n43" => "NOAA 43 (Miss Piggy)".into(),
        "n49" => "NOAA 49 (Gonzo)".into(),
        "n57" | "n67" => "NOAA G-IV".into(),
        r if r.starts_with('n') => format!("N{}RF", r.to_uppercase().get(1..).unwrap_or("")),
        r => r.to_uppercase(),
    }
}

/// Python `str.title()` for storm names (single/space-separated words).
fn title_case(s: &str) -> String {
    s.split_whitespace()
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                None => String::new(),
                Some(f) => f.to_uppercase().collect::<String>() + &c.as_str().to_lowercase(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ── HTTP crawl ───────────────────────────────────────────────────────────────

fn client(timeout: u64) -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent("noaa-recon-api/0.1")
        .timeout(std::time::Duration::from_secs(timeout))
        .build()?)
}

async fn fetch_bytes(client: &reqwest::Client, url: &str) -> Option<Vec<u8>> {
    match client.get(url).send().await.and_then(|r| r.error_for_status()) {
        Ok(r) => match r.bytes().await {
            Ok(b) => Some(b.to_vec()),
            Err(e) => {
                tracing::warn!("fetch failed {url}: {e}");
                None
            }
        },
        Err(e) => {
            tracing::warn!("fetch failed {url}: {e}");
            None
        }
    }
}

/// All `<a href>` targets, minus query/anchor and "/" (`list_hrefs`).
async fn list_hrefs(client: &reqwest::Client, url: &str) -> Vec<String> {
    let Some(bytes) = fetch_bytes(client, url).await else {
        return Vec::new();
    };
    let html = String::from_utf8_lossy(&bytes);
    let re = regex::Regex::new(r#"href="([^"]*)""#).unwrap();
    re.captures_iter(&html)
        .map(|c| c[1].to_string())
        .filter(|h| !h.starts_with('?') && h != "/")
        .collect()
}

// ── Storm-name extraction (PDF + netCDF attrs) ───────────────────────────────

/// `extract_from_pdf` -> (storm_name, storm_id). Best-effort text extraction.
fn extract_from_pdf(pdf_bytes: &[u8]) -> (Option<String>, Option<String>) {
    let full_text = match pdf_extract::extract_text_from_mem(pdf_bytes) {
        Ok(t) => t,
        Err(e) => {
            tracing::debug!("PDF parse error: {e}");
            return (None, None);
        }
    };
    let strip_ie = regex::Regex::new(r"\(i\.e\.,[^)]*\)").unwrap();
    let cleaned = strip_ie.replace_all(&full_text, "");

    let mut storm_name: Option<String> = None;
    if let Some(c) = regex::RegexBuilder::new(r"Storm[:\s]+([A-Z]{2,})\s+Flight\s+ID")
        .case_insensitive(true)
        .build()
        .unwrap()
        .captures(&full_text)
    {
        storm_name = Some(title_case(&c[1]));
    }
    if storm_name.is_none() {
        if let Some(c) = regex::Regex::new(r"NOAA\s*\d+\s+\d{3,4}[A-Z]\s+([A-Z]{2,})(?:\s|$)")
            .unwrap()
            .captures(&cleaned)
        {
            storm_name = Some(title_case(&c[1]));
        }
    }
    if storm_name.is_none() {
        if let Some(c) = regex::Regex::new(r"Mission\s*ID[:\s]+\d{3,4}[A-Z]\b.{0,60}?([A-Z]{3,})")
            .unwrap()
            .captures(&full_text)
        {
            let cand = &c[1];
            if !matches!(cand, "FLIGHT" | "MISSION" | "LANDING" | "TAKEOFF" | "REPORT") {
                storm_name = Some(title_case(cand));
            }
        }
    }

    let storm_id = regex::Regex::new(r"((?:AL|EP|CP|WP|IO|SH)\d{6})")
        .unwrap()
        .captures(&cleaned)
        .map(|c| c[1].to_uppercase());

    (storm_name, storm_id)
}

fn global_attr_string(ds: &netcdf::File, name: &str) -> Option<String> {
    match ds.attribute(name)?.value().ok()? {
        netcdf::AttributeValue::Str(s) => Some(s),
        _ => None,
    }
}

fn global_attr_f64(ds: &netcdf::File, name: &str) -> Option<f64> {
    use netcdf::AttributeValue::*;
    match ds.attribute(name)?.value().ok()? {
        Uchar(v) => Some(v as f64),
        Schar(v) => Some(v as f64),
        Ushort(v) => Some(v as f64),
        Short(v) => Some(v as f64),
        Uint(v) => Some(v as f64),
        Int(v) => Some(v as f64),
        Longlong(v) => Some(v as f64),
        Ulonglong(v) => Some(v as f64),
        Float(v) => Some(v as f64),
        Double(v) => Some(v),
        Str(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// `extract_storm_from_nc_attrs`: a Title-cased storm name from global attrs.
fn extract_storm_from_nc_attrs(path: &Path) -> Option<String> {
    let _guard = crate::services::goes::nc_lock();
    let ds = netcdf::open(path).ok()?;
    for attr in ["StormName", "storm_name", "storm"] {
        if let Some(val) = global_attr_string(&ds, attr) {
            let clean = title_case(val.trim());
            if clean.len() >= 2 && !matches!(clean.to_uppercase().as_str(), "NONE" | "N/A" | "UNKNOWN" | "TEST") {
                return Some(clean);
            }
        }
    }
    if let Some(title) = global_attr_string(&ds, "title") {
        if let Some(c) = regex::RegexBuilder::new(r"(?:hurricane|tropical storm|typhoon|cyclone)\s+([A-Z][a-z]+)")
            .case_insensitive(true)
            .build()
            .unwrap()
            .captures(&title)
        {
            return Some(title_case(&c[1]));
        }
    }
    None
}

// ── netCDF observation processing ────────────────────────────────────────────

struct ProcessResult {
    start_unix: i64,
    end_unix: i64,
    lat_min: f64,
    lat_max: f64,
    lon_min: f64,
    lon_max: f64,
    /// (unix_time, lat, lon, wind_kt, wind_dir, sfmr_kt, alt_m)
    observations: Vec<(i64, f64, f64, Option<f64>, Option<f64>, Option<f64>, Option<f64>)>,
}

/// Read a variable as f64 with `_FillValue`/`missing_value`/NaN -> NaN masking.
fn read_var_masked(ds: &netcdf::File, name: &str) -> Option<Vec<f64>> {
    let var = ds.variable(name)?;
    let fill = var_attr_f64(&var, "_FillValue");
    let missing = var_attr_f64(&var, "missing_value");
    let raw: Vec<f64> = var.get_values::<f64, _>(..).ok()?;
    Some(
        raw.into_iter()
            .map(|v| {
                if v.is_nan() || fill.map(|f| v == f).unwrap_or(false) || missing.map(|m| v == m).unwrap_or(false) {
                    f64::NAN
                } else {
                    v
                }
            })
            .collect(),
    )
}

fn var_attr_f64(var: &netcdf::Variable, name: &str) -> Option<f64> {
    use netcdf::AttributeValue::*;
    match var.attribute_value(name)?.ok()? {
        Uchar(v) => Some(v as f64),
        Schar(v) => Some(v as f64),
        Ushort(v) => Some(v as f64),
        Short(v) => Some(v as f64),
        Uint(v) => Some(v as f64),
        Int(v) => Some(v as f64),
        Longlong(v) => Some(v as f64),
        Ulonglong(v) => Some(v as f64),
        Float(v) => Some(v as f64),
        Double(v) => Some(v),
        _ => None,
    }
}

fn first_var_masked(ds: &netcdf::File, names: &[&str]) -> Option<Vec<f64>> {
    names.iter().find_map(|n| read_var_masked(ds, n))
}

/// Port of `process_nc_file`.
fn process_nc_file(path: &Path, mission_id: &str) -> Option<ProcessResult> {
    let (start_unix, time_raw, lat, lon, ws_kt, wd, sfmr_kt, alt_m) = {
        let _guard = crate::services::goes::nc_lock();
        let ds = netcdf::open(path).ok()?;
        let start_unix = global_attr_f64(&ds, "StartTime").unwrap_or(0.0) as i64;
        let time_raw = read_var_masked(&ds, "Time");
        if time_raw.is_none() || start_unix == 0 {
            tracing::warn!("{mission_id}: missing Time variable or StartTime attribute");
            return None;
        }
        let lat = first_var_masked(&ds, &["LATref", "LatGPS.2", "LatGPS.3", "LatGPS.1"]);
        let lon = first_var_masked(&ds, &["LONref", "LonGPS.2", "LonGPS.3", "LonGPS.1"]);
        if lat.is_none() || lon.is_none() {
            tracing::warn!("{mission_id}: no lat/lon variables found");
            return None;
        }
        let ws_kt = read_var_masked(&ds, "WSkt.d");
        let wd = read_var_masked(&ds, "WD.d");
        let sfmr_ms = first_var_masked(&ds, &["NSfmrWS.1", "SFMRWSref", "SfmrWS.1", "ASfmrWS.1"]);
        let sfmr_kt = sfmr_ms.map(|v| v.into_iter().map(|x| x * MS_TO_KT).collect::<Vec<_>>());
        let alt_m = first_var_masked(&ds, &["ALTref", "AltGPS.2", "AltGPS.3", "AltGPS.1", "AltBCADDU.1"]);
        (start_unix, time_raw.unwrap(), lat.unwrap(), lon.unwrap(), ws_kt, wd, sfmr_kt, alt_m)
    };

    let n = time_raw.len().min(lat.len()).min(lon.len());
    let valid_idx: Vec<usize> = (0..n)
        .filter(|&i| time_raw[i].is_finite() && lat[i].is_finite() && lon[i].is_finite())
        .collect();
    if valid_idx.len() < 10 {
        tracing::warn!("{mission_id}: fewer than 10 valid points, skipping");
        return None;
    }

    let at = |v: &Option<Vec<f64>>, i: usize| -> Option<f64> {
        v.as_ref().and_then(|arr| arr.get(i)).copied().filter(|x| x.is_finite())
    };

    let mut observations = Vec::new();
    let (mut lats, mut lons) = (Vec::new(), Vec::new());
    for &i in valid_idx.iter().step_by(DECIMATION) {
        let (t_raw, la, lo) = (time_raw[i], lat[i], lon[i]);
        if !t_raw.is_finite() || !la.is_finite() || !lo.is_finite() {
            continue;
        }
        let t_unix = start_unix + t_raw as i64;
        if !(-90.0..=90.0).contains(&la) || !(-180.0..=180.0).contains(&lo) {
            continue;
        }
        // Skip (0,0) "null island" GPS/INS-dropout sentinel.
        if la.abs() < 0.01 && lo.abs() < 0.01 {
            continue;
        }
        let mut ws = at(&ws_kt, i);
        if let Some(w) = ws {
            if !(0.0..=300.0).contains(&w) {
                ws = None;
            }
        }
        let mut wdv = at(&wd, i);
        if let Some(d) = wdv {
            if !(0.0..=360.0).contains(&d) {
                wdv = None;
            }
        }
        let sf = at(&sfmr_kt, i);
        let al = at(&alt_m, i);
        observations.push((t_unix, la, lo, ws, wdv, sf, al));
        lats.push(la);
        lons.push(lo);
    }
    if observations.len() < 5 {
        tracing::warn!("{mission_id}: not enough valid sampled obs");
        return None;
    }
    let unix_times: Vec<i64> = observations.iter().map(|o| o.0).collect();
    Some(ProcessResult {
        start_unix: *unix_times.iter().min().unwrap(),
        end_unix: *unix_times.iter().max().unwrap(),
        lat_min: lats.iter().cloned().fold(f64::INFINITY, f64::min),
        lat_max: lats.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
        lon_min: lons.iter().cloned().fold(f64::INFINITY, f64::min),
        lon_max: lons.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
        observations,
    })
}

// ── Mission discovery + aircraft ─────────────────────────────────────────────

async fn get_mission_list(client: &reqwest::Client, year: i64) -> Vec<String> {
    let url = format!("{BASE_URL}/{year}/MET/");
    let re = regex::Regex::new(r"^\d{8}[A-Z]\d+$").unwrap();
    list_hrefs(client, &url)
        .await
        .into_iter()
        .map(|h| h.trim_end_matches('/').to_string())
        .filter(|h| re.is_match(h))
        .collect()
}

async fn get_best_nc_file(
    client: &reqwest::Client,
    mission_id: &str,
    mission_url: &str,
) -> Option<(String, String)> {
    let re = regex::RegexBuilder::new(&format!(r"^{}_([A-Z])\.nc$", regex::escape(mission_id)))
        .case_insensitive(true)
        .build()
        .unwrap();
    let mut best: Option<(String, String)> = None;
    for h in list_hrefs(client, mission_url).await {
        let name = h.rsplit('/').next().unwrap_or(&h).to_string();
        if let Some(c) = re.captures(&name) {
            let letter = c[1].to_uppercase();
            if best.as_ref().map(|(l, _)| &letter > l).unwrap_or(true) {
                best = Some((letter, name));
            }
        }
    }
    best.map(|(letter, name)| (name, letter))
}

async fn get_aircraft_info(client: &reqwest::Client, mission_url: &str) -> (String, String) {
    let Some(bytes) = fetch_bytes(client, &format!("{mission_url}aampsrc")).await else {
        return ("Unknown Aircraft".into(), String::new());
    };
    let text = String::from_utf8_lossy(&bytes);
    match regex::RegexBuilder::new(r"TAILNUM=(\S+)")
        .case_insensitive(true)
        .build()
        .unwrap()
        .captures(&text)
    {
        Some(c) => {
            let raw = c[1].to_lowercase();
            let raw = raw.trim();
            (tailnum_name(raw), raw.to_uppercase())
        }
        None => ("Unknown Aircraft".into(), String::new()),
    }
}

// ── Per-mission harvest ──────────────────────────────────────────────────────

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Port of `harvest_mission`. Returns whether the mission was (re)ingested.
async fn harvest_mission(
    http: &reqwest::Client,
    nc_http: &reqwest::Client,
    conn: &Connection,
    year: i64,
    mission_id: &str,
    force: bool,
) -> anyhow::Result<bool> {
    let mission_url = format!("{BASE_URL}/{year}/MET/{mission_id}/");

    let (nc_filename, nc_version) = match get_best_nc_file(http, mission_id, &mission_url).await {
        Some(v) => v,
        None => return Ok(false),
    };

    let existing: Option<String> = conn
        .query_row("SELECT nc_version FROM missions WHERE mission_id = ?1", [mission_id], |r| {
            r.get::<_, Option<String>>(0)
        })
        .ok()
        .flatten();
    if existing.as_deref() == Some(nc_version.as_str()) && !force {
        return Ok(false);
    }
    tracing::info!("{mission_id}: harvesting (NC version={nc_version})");

    // Storm name/id: PDF first, then netCDF attrs, then the training bucket.
    let (mut storm_name, storm_id) = {
        let hrefs = list_hrefs(http, &mission_url).await;
        match hrefs.iter().find(|h| h.to_lowercase().ends_with(".pdf")) {
            Some(pdf) => {
                let pdf_name = pdf.rsplit('/').next().unwrap_or(pdf);
                match fetch_bytes(http, &format!("{mission_url}{pdf_name}")).await {
                    Some(b) => extract_from_pdf(&b),
                    None => (None, None),
                }
            }
            None => (None, None),
        }
    };

    let (aircraft, tail_num) = get_aircraft_info(http, &mission_url).await;

    let nc_url = format!("{mission_url}{nc_filename}");
    let Some(nc_data) = fetch_bytes(nc_http, &nc_url).await else {
        tracing::warn!("{mission_id}: download failed");
        return Ok(false);
    };

    // Spill to a temp file (netcdf opens a path). Close the writer before opening.
    let tmp = tempfile::Builder::new().suffix(".nc").tempfile()?;
    let (mut file, tmp_path) = tmp.into_parts();
    file.write_all(&nc_data)?;
    file.flush()?;
    drop(file);

    if storm_name.is_none() {
        storm_name = extract_storm_from_nc_attrs(&tmp_path);
    }
    let storm_name = storm_name.unwrap_or_else(|| TRAINING_BUCKET_NAME.to_string());

    let result = process_nc_file(&tmp_path, mission_id);
    drop(tmp_path); // deletes the temp file
    let result = match result {
        Some(r) => r,
        None => return Ok(false),
    };

    let flight_date = DateTime::<Utc>::from_timestamp(result.start_unix, 0)
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_default();

    conn.execute_batch("BEGIN")?;
    let res = (|| -> rusqlite::Result<()> {
        conn.execute(
            "INSERT INTO missions \
             (mission_id, year, storm_name, storm_id, aircraft, tail_num, flight_date, \
              start_unix, end_unix, nc_version, source_url, lat_min, lat_max, lon_min, lon_max, obs_count, fetched_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17) \
             ON CONFLICT(mission_id) DO UPDATE SET \
              storm_name=excluded.storm_name, storm_id=excluded.storm_id, aircraft=excluded.aircraft, \
              tail_num=excluded.tail_num, flight_date=excluded.flight_date, start_unix=excluded.start_unix, \
              end_unix=excluded.end_unix, nc_version=excluded.nc_version, source_url=excluded.source_url, \
              lat_min=excluded.lat_min, lat_max=excluded.lat_max, lon_min=excluded.lon_min, \
              lon_max=excluded.lon_max, obs_count=excluded.obs_count, fetched_at=excluded.fetched_at",
            rusqlite::params![
                mission_id, year, storm_name, storm_id, aircraft, tail_num, flight_date,
                result.start_unix, result.end_unix, nc_version, nc_url,
                result.lat_min, result.lat_max, result.lon_min, result.lon_max,
                result.observations.len() as i64, now_unix(),
            ],
        )?;
        conn.execute("DELETE FROM observations WHERE mission_id = ?1", [mission_id])?;
        let mut stmt = conn.prepare(
            "INSERT INTO observations (mission_id, seq_num, unix_time, lat, lon, wind_kt, wind_dir, sfmr_kt, alt_m) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        )?;
        for (seq, (t, la, lo, ws, wdv, sf, al)) in result.observations.iter().enumerate() {
            stmt.execute(rusqlite::params![mission_id, seq as i64, t, la, lo, ws, wdv, sf, al])?;
        }
        Ok(())
    })();
    match res {
        Ok(()) => {
            conn.execute_batch("COMMIT")?;
            tracing::info!("{mission_id}: stored {} obs (version {nc_version})", result.observations.len());
            Ok(true)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e.into())
        }
    }
}

// ── Reconciliation (haversine track-matching against the storms DB) ───────────

fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let r = 6371.0;
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dphi = (lat2 - lat1).to_radians();
    let dlambda = (lon2 - lon1).to_radians();
    let a = (dphi / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dlambda / 2.0).sin().powi(2);
    2.0 * r * a.sqrt().asin()
}

/// A mission's decimated (lat, lon) obs, excluding (0,0) null-island readings.
fn mission_track_points(conn: &Connection, mission_id: &str) -> Vec<(f64, f64)> {
    let mut stmt = conn
        .prepare(
            "SELECT lat, lon FROM observations WHERE mission_id = ?1 \
             AND NOT (ABS(lat) < 0.01 AND ABS(lon) < 0.01)",
        )
        .unwrap();
    let rows = stmt
        .query_map([mission_id], |r| Ok((r.get::<_, f64>(0)?, r.get::<_, f64>(1)?)))
        .unwrap();
    rows.filter_map(|r| r.ok()).collect()
}

fn mission_min_distance_km(points: &[(f64, f64)], lat: f64, lon: f64) -> Option<f64> {
    points
        .iter()
        .map(|&(la, lo)| haversine_km(la, lo, lat, lon))
        .fold(None, |acc, d| Some(acc.map_or(d, |a: f64| a.min(d))))
}

/// Closest approach of `points` to the storm's track point nearest in time to
/// `mid_unix`, or None if outside the time window.
fn storm_track_distance_km(
    storms_conn: &Connection,
    points: &[(f64, f64)],
    storm_id: i64,
    mid_unix: f64,
) -> Option<f64> {
    let mid_dt = DateTime::<Utc>::from_timestamp(mid_unix as i64, 0)?
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let row = storms_conn
        .query_row(
            "SELECT datetime_utc, lat, lon FROM track_points WHERE storm_id = ?1 \
             ORDER BY ABS(julianday(datetime_utc) - julianday(?2)) LIMIT 1",
            rusqlite::params![storm_id, mid_dt],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?, r.get::<_, f64>(2)?)),
        )
        .ok()?;
    let point_dt = DateTime::parse_from_rfc3339(&row.0.replace('Z', "+00:00")).ok()?;
    let hours_off = (point_dt.timestamp() as f64 - mid_unix).abs() / 3600.0;
    if hours_off > MAX_STORM_MATCH_TIME_HOURS {
        return None;
    }
    mission_min_distance_km(points, row.1, row.2)
}

/// storms row (id, atcf_id, name) whose track passed nearest a mission's path.
fn find_matching_storm(
    storms_conn: &Connection,
    recon_conn: &Connection,
    mission_id: &str,
    year: i64,
    mid_unix: f64,
) -> Option<(i64, String, String)> {
    let points = mission_track_points(recon_conn, mission_id);
    if points.is_empty() {
        return None;
    }
    let mut stmt = storms_conn
        .prepare("SELECT id, atcf_id, name FROM storms WHERE year IN (?1, ?2, ?3)")
        .unwrap();
    let rows = stmt
        .query_map(rusqlite::params![year - 1, year, year + 1], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap();
    let candidates: Vec<(i64, String, String)> = rows.filter_map(|r| r.ok()).collect();

    let mut best: Option<((i64, String, String), f64)> = None;
    for s in candidates {
        if let Some(d) = storm_track_distance_km(storms_conn, &points, s.0, mid_unix) {
            if d <= MAX_STORM_MATCH_DISTANCE_KM && best.as_ref().map(|(_, bd)| d < *bd).unwrap_or(true) {
                best = Some((s, d));
            }
        }
    }
    best.map(|(s, _)| s)
}

fn apply_manual_storm_name_corrections(conn: &Connection) -> rusqlite::Result<usize> {
    let mut fixed = 0;
    for (mission_id, storm_name) in MANUAL_STORM_NAME_CORRECTIONS {
        fixed += conn.execute(
            "UPDATE missions SET storm_name = ?1, storm_id = NULL WHERE mission_id = ?2 AND storm_name != ?1",
            rusqlite::params![storm_name, mission_id],
        )?;
    }
    Ok(fixed)
}

fn clean_null_island_observations(conn: &Connection) -> rusqlite::Result<usize> {
    let affected: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT DISTINCT mission_id FROM observations WHERE ABS(lat) < 0.01 AND ABS(lon) < 0.01",
        )?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        rows.filter_map(|r| r.ok()).collect()
    };
    let mut removed = 0;
    for mid in &affected {
        removed += conn.execute(
            "DELETE FROM observations WHERE mission_id = ?1 AND ABS(lat) < 0.01 AND ABS(lon) < 0.01",
            [mid],
        )?;
        let (c, la_mn, la_mx, lo_mn, lo_mx): (i64, Option<f64>, Option<f64>, Option<f64>, Option<f64>) =
            conn.query_row(
                "SELECT COUNT(*), MIN(lat), MAX(lat), MIN(lon), MAX(lon) FROM observations WHERE mission_id = ?1",
                [mid],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )?;
        conn.execute(
            "UPDATE missions SET obs_count=?1, lat_min=?2, lat_max=?3, lon_min=?4, lon_max=?5 WHERE mission_id=?6",
            rusqlite::params![c, la_mn, la_mx, lo_mn, lo_mx, mid],
        )?;
    }
    Ok(removed)
}

fn migrate_unknown_storm_names(conn: &Connection) -> rusqlite::Result<usize> {
    let re = regex::Regex::new(r"^\d{8}[A-Z]\d+$").unwrap();
    let ids: Vec<String> = {
        let mut stmt = conn.prepare("SELECT mission_id, storm_name FROM missions")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        rows.filter_map(|r| r.ok())
            .filter(|(_, name)| re.is_match(name))
            .map(|(mid, _)| mid)
            .collect()
    };
    for mid in &ids {
        conn.execute(
            "UPDATE missions SET storm_name = ?1 WHERE mission_id = ?2",
            rusqlite::params![TRAINING_BUCKET_NAME, mid],
        )?;
    }
    Ok(ids.len())
}

fn rename_legacy_training_bucket(conn: &Connection) -> rusqlite::Result<usize> {
    let mut renamed = 0;
    for legacy in LEGACY_TRAINING_BUCKET_NAMES {
        renamed += conn.execute(
            "UPDATE missions SET storm_name = ?1 WHERE storm_name = ?2",
            rusqlite::params![TRAINING_BUCKET_NAME, legacy],
        )?;
    }
    Ok(renamed)
}

fn reconcile_storm_ids(conn: &Connection) -> rusqlite::Result<usize> {
    let groups: Vec<(String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT storm_id, \
               COUNT(DISTINCT CASE WHEN storm_name != ?1 THEN storm_name END) AS real_name_count, \
               MAX(CASE WHEN storm_name != ?1 THEN storm_name END) AS real_name \
             FROM missions WHERE storm_id IS NOT NULL AND storm_id != '' \
             GROUP BY storm_id \
             HAVING real_name_count = 1 AND SUM(CASE WHEN storm_name = ?1 THEN 1 ELSE 0 END) > 0",
        )?;
        let rows = stmt.query_map([TRAINING_BUCKET_NAME], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(2)?))
        })?;
        rows.filter_map(|r| r.ok()).collect()
    };
    let mut fixed = 0;
    for (storm_id, real_name) in &groups {
        fixed += conn.execute(
            "UPDATE missions SET storm_name = ?1 WHERE storm_id = ?2 AND storm_name = ?3",
            rusqlite::params![real_name, storm_id, TRAINING_BUCKET_NAME],
        )?;
    }
    Ok(fixed)
}

fn reconcile_junk_storm_buckets(conn: &Connection, storms_conn: &Connection) -> rusqlite::Result<usize> {
    let junk = junk_storm_names();
    let manual_ids: Vec<&str> = MANUAL_STORM_NAME_CORRECTIONS.iter().map(|(m, _)| *m).collect();
    let junk_ph = vec!["?"; junk.len()].join(",");
    let mut sql = format!(
        "SELECT mission_id, year, storm_id, start_unix, end_unix FROM missions WHERE UPPER(storm_name) IN ({junk_ph})"
    );
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = junk.iter().map(|s| Box::new(s.clone()) as _).collect();
    if !manual_ids.is_empty() {
        let mp = vec!["?"; manual_ids.len()].join(",");
        sql += &format!(" AND mission_id NOT IN ({mp})");
        for m in &manual_ids {
            params.push(Box::new(m.to_string()));
        }
    }
    let rows: Vec<(String, i64, Option<String>, i64, i64)> = {
        let mut stmt = conn.prepare(&sql)?;
        let refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(refs.as_slice(), |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?;
        rows.filter_map(|r| r.ok()).collect()
    };

    let sid_re = regex::Regex::new(r"^[A-Z]{2}\d{2}(\d{4})$").unwrap();
    let mut fixed = 0;
    for (mission_id, year, storm_id, start_unix, end_unix) in rows {
        // Tier 1: trust an internally-consistent storm_id (embedded year matches).
        let mut matched: Option<(String, String)> = None; // (name, atcf_id)
        let sid = storm_id.unwrap_or_default().trim().to_uppercase();
        if let Some(c) = sid_re.captures(&sid) {
            if c[1].parse::<i64>().ok() == Some(year) {
                matched = storms_conn
                    .query_row(
                        "SELECT name, atcf_id FROM storms WHERE atcf_id = ?1",
                        [&sid],
                        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                    )
                    .ok();
            }
        }
        // Tier 2: position match against every storm's track.
        if matched.is_none() {
            let mid_unix = (start_unix + end_unix) as f64 / 2.0;
            if let Some((_, atcf, name)) = find_matching_storm(storms_conn, conn, &mission_id, year, mid_unix) {
                matched = Some((name, atcf));
            }
        }
        if let Some((name, atcf)) = matched {
            conn.execute(
                "UPDATE missions SET storm_name = ?1, storm_id = ?2 WHERE mission_id = ?3",
                rusqlite::params![title_case(&name), atcf, mission_id],
            )?;
            fixed += 1;
        }
    }
    Ok(fixed)
}

fn reconcile_mismatched_storm_names(conn: &Connection, storms_conn: &Connection) -> rusqlite::Result<usize> {
    let junk = junk_storm_names();
    let junk_ph = vec!["?"; junk.len()].join(",");
    let named: Vec<(String, i64, String, i64, i64)> = {
        let mut stmt = conn.prepare(&format!(
            "SELECT mission_id, year, storm_name, start_unix, end_unix FROM missions \
             WHERE UPPER(storm_name) NOT IN ({junk_ph})"
        ))?;
        let refs: Vec<&dyn rusqlite::types::ToSql> = junk.iter().map(|s| s as &dyn rusqlite::types::ToSql).collect();
        let rows = stmt.query_map(refs.as_slice(), |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?;
        rows.filter_map(|r| r.ok()).collect()
    };

    let mut demoted = 0;
    for (mission_id, year, storm_name, start_unix, end_unix) in named {
        let points = mission_track_points(conn, &mission_id);
        if points.is_empty() {
            continue;
        }
        let cand_ids: Vec<i64> = {
            let mut stmt = storms_conn.prepare(
                "SELECT id FROM storms WHERE name = ?1 COLLATE NOCASE AND year IN (?2, ?3, ?4)",
            )?;
            let rows = stmt.query_map(
                rusqlite::params![storm_name, year - 1, year, year + 1],
                |r| r.get(0),
            )?;
            rows.filter_map(|r| r.ok()).collect()
        };
        let mid_unix = (start_unix + end_unix) as f64 / 2.0;
        let confirmed = cand_ids.iter().any(|&id| {
            storm_track_distance_km(storms_conn, &points, id, mid_unix)
                .map(|d| d <= MAX_STORM_MATCH_DISTANCE_KM)
                .unwrap_or(false)
        });
        if !confirmed {
            conn.execute(
                "UPDATE missions SET storm_name = ?1, storm_id = NULL WHERE mission_id = ?2",
                rusqlite::params![TRAINING_BUCKET_NAME, mission_id],
            )?;
            demoted += 1;
        }
    }
    Ok(demoted)
}

// ── Orchestration ────────────────────────────────────────────────────────────

/// Full recon ingest (`run_ingest`). `years` defaults to [current-1, current].
pub async fn run_ingest(
    recon_db: &Path,
    storms_db: &Path,
    years: Option<Vec<i64>>,
    force: bool,
) -> anyhow::Result<Value> {
    let years = years.unwrap_or_else(|| {
        let y = Utc::now().year() as i64;
        vec![y - 1, y]
    });

    let conn = recon_met::get_connection(recon_db)?;
    let http = client(HTTP_TIMEOUT_SECS)?;
    let nc_http = client(NC_TIMEOUT_SECS)?;

    let mut summary = Map::new();
    let mut years_map = Map::new();
    let (mut ingested, mut skipped, mut errors) = (0i64, 0i64, 0i64);

    for year in &years {
        let mission_ids = get_mission_list(&http, *year).await;
        let mut year_ingested = 0;
        for mission_id in &mission_ids {
            match harvest_mission(&http, &nc_http, &conn, *year, mission_id, force).await {
                Ok(true) => {
                    year_ingested += 1;
                    ingested += 1;
                }
                Ok(false) => skipped += 1,
                Err(e) => {
                    tracing::error!("{mission_id}: error during harvest: {e}");
                    errors += 1;
                }
            }
        }
        years_map.insert(year.to_string(), json!(year_ingested));
    }

    // Reconciliation passes (need the storms DB for track matching).
    let storms_conn = storms::get_connection(storms_db)?;
    let mut counts = Map::new();
    let cleaned = clean_null_island_observations(&conn)?;
    if cleaned > 0 { counts.insert("null_island_obs_removed".into(), json!(cleaned)); }
    let corrected = apply_manual_storm_name_corrections(&conn)?;
    if corrected > 0 { counts.insert("manual_corrections_applied".into(), json!(corrected)); }
    let legacy = migrate_unknown_storm_names(&conn)?;
    if legacy > 0 { counts.insert("legacy_names_fixed".into(), json!(legacy)); }
    let renamed = rename_legacy_training_bucket(&conn)?;
    if renamed > 0 { counts.insert("legacy_bucket_renamed".into(), json!(renamed)); }
    let recon_ids = reconcile_storm_ids(&conn)?;
    if recon_ids > 0 { counts.insert("storm_ids_reconciled".into(), json!(recon_ids)); }
    let mismatched = reconcile_mismatched_storm_names(&conn, &storms_conn)?;
    if mismatched > 0 { counts.insert("mismatched_names_demoted".into(), json!(mismatched)); }
    let junk = reconcile_junk_storm_buckets(&conn, &storms_conn)?;
    if junk > 0 { counts.insert("junk_buckets_reconciled".into(), json!(junk)); }

    let total: i64 = conn.query_row("SELECT COUNT(*) FROM missions", [], |r| r.get(0))?;

    summary.insert("years".into(), Value::Object(years_map));
    summary.insert("ingested".into(), json!(ingested));
    summary.insert("skipped".into(), json!(skipped));
    summary.insert("errors".into(), json!(errors));
    for (k, v) in counts {
        summary.insert(k, v);
    }
    summary.insert("total_missions".into(), json!(total));
    Ok(Value::Object(summary))
}

/// Re-ingest a single recon MET mission (crawl its `acdata/{year}/MET/{id}/`
/// dir, download + decimate its netCDF, extract its storm name) and then run
/// the reconciliation passes so the freshly-ingested mission gets track-matched
/// against the storms DB. Backs the admin console's per-flight "pull recon
/// data" button — the recon half of recovering a radar-only flight that landed
/// before its MET data uploaded. `force`-harvests so an already-known mission
/// is still refreshed.
pub async fn reingest_mission(
    recon_db: &Path,
    storms_db: &Path,
    mission_id: &str,
) -> anyhow::Result<Value> {
    let year: i64 = mission_id
        .get(0..4)
        .and_then(|y| y.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("mission_id '{mission_id}' has no leading YYYY"))?;
    let conn = recon_met::get_connection(recon_db)?;
    let http = client(HTTP_TIMEOUT_SECS)?;
    let nc_http = client(NC_TIMEOUT_SECS)?;

    let harvested = harvest_mission(&http, &nc_http, &conn, year, mission_id, true).await?;

    // Same reconciliation passes as run_ingest, so the new mission is matched
    // to a storm rather than left in the training bucket.
    let storms_conn = storms::get_connection(storms_db)?;
    clean_null_island_observations(&conn)?;
    apply_manual_storm_name_corrections(&conn)?;
    migrate_unknown_storm_names(&conn)?;
    rename_legacy_training_bucket(&conn)?;
    reconcile_storm_ids(&conn)?;
    reconcile_mismatched_storm_names(&conn, &storms_conn)?;
    reconcile_junk_storm_buckets(&conn, &storms_conn)?;

    let row: Option<(String, Option<String>)> = conn
        .query_row(
            "SELECT storm_name, storm_id FROM missions WHERE mission_id = ?1",
            [mission_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();
    let (storm_name, storm_id) = match row {
        Some((n, i)) => (Some(n), i),
        None => (None, None),
    };

    Ok(json!({
        "mission_id": mission_id,
        "harvested": harvested,
        "found": storm_name.is_some(),
        "storm_name": storm_name,
        "storm_id": storm_id,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_case_storm_names() {
        assert_eq!(title_case("DORIAN"), "Dorian");
        assert_eq!(title_case("miss piggy"), "Miss Piggy");
    }

    #[test]
    fn haversine_known_distance() {
        // ~equator degree of longitude ~111km
        let d = haversine_km(0.0, 0.0, 0.0, 1.0);
        assert!((d - 111.19).abs() < 1.0, "d={d}");
    }

    #[test]
    fn tailnum_mapping() {
        assert_eq!(tailnum_name("n42"), "NOAA 42 (Kermit)");
        assert_eq!(tailnum_name("n57"), "NOAA G-IV");
    }
}
