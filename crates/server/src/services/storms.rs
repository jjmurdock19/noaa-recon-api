//! Historical storm-track database — READ path, port of the query helpers in
//! `app/services/storms.py`.
//!
//! Scope note: the HURDAT2/ATCF **ingest** pipeline (parsing, upsert,
//! `run_ingest`) is intentionally NOT ported yet. Both the Python and Rust
//! servers read the same `data/storms.sqlite`, so for local testing the DB is
//! populated by the existing `scripts/ingest_storms.py` / nightly timer. The
//! admin "force update" button will shell out to that (or get a native port
//! later). This module is everything `app/routers/storms.py` needs to serve.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Row};
use serde::Serialize;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS storms (
    id INTEGER PRIMARY KEY,
    basin TEXT NOT NULL,
    atcf_id TEXT NOT NULL UNIQUE,
    year INTEGER NOT NULL,
    name TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_storms_year ON storms(year);
CREATE INDEX IF NOT EXISTS idx_storms_year_name ON storms(year, name);

CREATE TABLE IF NOT EXISTS track_points (
    id INTEGER PRIMARY KEY,
    storm_id INTEGER NOT NULL REFERENCES storms(id) ON DELETE CASCADE,
    datetime_utc TEXT NOT NULL,
    status TEXT NOT NULL,
    category TEXT NOT NULL,
    lat REAL NOT NULL,
    lon REAL NOT NULL,
    wind_kt INTEGER,
    pressure_mb INTEGER
);
CREATE INDEX IF NOT EXISTS idx_track_storm_dt ON track_points(storm_id, datetime_utc);

-- Tracks which HURDAT2 file (per basin) is currently reflected in `storms`,
-- so check_hurdat_updates can tell a fresh NOAA release apart from one it's
-- already ingested without re-downloading the file every time it checks.
CREATE TABLE IF NOT EXISTS hurdat_sources (
    basin TEXT PRIMARY KEY,
    filename TEXT NOT NULL,
    last_modified TEXT NOT NULL,
    checked_at TEXT NOT NULL
);
";

/// A storm header row (subset selected by the query helpers).
#[derive(Debug, Clone, Serialize)]
pub struct Storm {
    pub id: i64,
    pub atcf_id: String,
    pub basin: String,
    pub year: i64,
    pub name: String,
}

/// A best-track fix. Field order/None-handling matches `_row_to_point` in the
/// Python router.
#[derive(Debug, Clone, Serialize)]
pub struct TrackPoint {
    pub datetime_utc: String,
    pub status: String,
    pub category: String,
    pub lat: f64,
    pub lon: f64,
    pub wind_kt: Option<i64>,
    pub pressure_mb: Option<i64>,
}

impl TrackPoint {
    fn from_row(row: &Row) -> rusqlite::Result<Self> {
        Ok(Self {
            datetime_utc: row.get("datetime_utc")?,
            status: row.get("status")?,
            category: row.get("category")?,
            lat: row.get("lat")?,
            lon: row.get("lon")?,
            wind_kt: row.get("wind_kt")?,
            pressure_mb: row.get("pressure_mb")?,
        })
    }
}

pub fn get_connection(db_path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(db_path)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

pub fn list_years(conn: &Connection) -> rusqlite::Result<Vec<i64>> {
    let mut stmt = conn.prepare("SELECT DISTINCT year FROM storms ORDER BY year")?;
    let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
    rows.collect()
}

pub fn list_storms_for_year(conn: &Connection, year: i64) -> rusqlite::Result<Vec<Storm>> {
    let mut stmt = conn
        .prepare("SELECT 0 AS id, atcf_id, basin, year, name FROM storms WHERE year = ?1 ORDER BY name")?;
    let rows = stmt.query_map([year], storm_from_row)?;
    rows.collect()
}

pub fn find_storms(
    conn: &Connection,
    year: i64,
    name: &str,
    basin: Option<&str>,
) -> rusqlite::Result<Vec<Storm>> {
    let name = name.to_uppercase();
    match basin {
        Some(b) => {
            let mut stmt = conn.prepare(
                "SELECT id, atcf_id, basin, year, name FROM storms \
                 WHERE year = ?1 AND name = ?2 AND basin = ?3",
            )?;
            let rows = stmt.query_map(
                rusqlite::params![year, name, b.to_uppercase()],
                storm_from_row,
            )?;
            rows.collect()
        }
        None => {
            let mut stmt = conn.prepare(
                "SELECT id, atcf_id, basin, year, name FROM storms WHERE year = ?1 AND name = ?2",
            )?;
            let rows = stmt.query_map(rusqlite::params![year, name], storm_from_row)?;
            rows.collect()
        }
    }
}

pub fn get_track(conn: &Connection, storm_id: i64) -> rusqlite::Result<Vec<TrackPoint>> {
    let mut stmt = conn.prepare(
        "SELECT datetime_utc, status, category, lat, lon, wind_kt, pressure_mb \
         FROM track_points WHERE storm_id = ?1 ORDER BY datetime_utc",
    )?;
    let rows = stmt.query_map([storm_id], TrackPoint::from_row)?;
    rows.collect()
}

pub fn find_nearest_point(
    conn: &Connection,
    storm_id: i64,
    target_datetime_utc: &str,
) -> rusqlite::Result<Option<TrackPoint>> {
    conn.query_row(
        "SELECT datetime_utc, status, category, lat, lon, wind_kt, pressure_mb \
         FROM track_points WHERE storm_id = ?1 \
         ORDER BY ABS(julianday(datetime_utc) - julianday(?2)) LIMIT 1",
        rusqlite::params![storm_id, target_datetime_utc],
        TrackPoint::from_row,
    )
    .optional()
}

fn storm_from_row(row: &Row) -> rusqlite::Result<Storm> {
    Ok(Storm {
        id: row.get("id")?,
        atcf_id: row.get("atcf_id")?,
        basin: row.get("basin")?,
        year: row.get("year")?,
        name: row.get("name")?,
    })
}

// ════════════════════════════════════════════════════════════════════════════
// INGEST — port of the HURDAT2 + ATCF pipeline in app/services/storms.py.
// Two authoritative, structured sources stitched together (see the Python
// module docstring): HURDAT2 (annual reconciled best-track) + ATCF b-decks
// (near-real-time, fills the gap to today). Pure HTTP + text/gzip parsing.
// ════════════════════════════════════════════════════════════════════════════

use std::io::Read;

use chrono::Datelike;
use serde_json::{json, Value};

const HURDAT_URLS: [(&str, &str); 2] = [
    ("AL", "https://www.nhc.noaa.gov/data/hurdat/hurdat2-1851-2024-040425.txt"),
    ("EP", "https://www.nhc.noaa.gov/data/hurdat/hurdat2-nepac-1949-2023-042624.txt"),
];
const ATCF_BASINS: [(&str, &str); 3] = [("AL", "al"), ("EP", "ep"), ("CP", "cp")];

// ── Weekly HURDAT2 release check ────────────────────────────────────────────
// NHC republishes each basin's HURDAT2 file under a new, dated filename every
// time it reconciles a season's best track (see check_hurdat_updates below).
// There's no CP entry here because NHC doesn't publish a HURDAT2 file for the
// central Pacific — CP storms stay ATCF-only, same as today.
const HURDAT_INDEX_URL: &str = "https://www.nhc.noaa.gov/data/hurdat/";
const HURDAT_BASIN_PREFIXES: [(&str, &str); 2] = [("AL", "1851"), ("EP", "nepac")];

fn status_label(status: &str) -> &str {
    match status {
        "TD" => "Tropical Depression",
        "TS" => "Tropical Storm",
        "SD" => "Subtropical Depression",
        "SS" => "Subtropical Storm",
        "EX" => "Extratropical Cyclone",
        "LO" => "Low",
        "WV" => "Tropical Wave",
        "DB" => "Disturbance",
        other => other,
    }
}

const ORDINAL_WORDS: [&str; 30] = [
    "ONE", "TWO", "THREE", "FOUR", "FIVE", "SIX", "SEVEN", "EIGHT", "NINE", "TEN",
    "ELEVEN", "TWELVE", "THIRTEEN", "FOURTEEN", "FIFTEEN", "SIXTEEN", "SEVENTEEN",
    "EIGHTEEN", "NINETEEN", "TWENTY", "TWENTYONE", "TWENTYTWO", "TWENTYTHREE",
    "TWENTYFOUR", "TWENTYFIVE", "TWENTYSIX", "TWENTYSEVEN", "TWENTYEIGHT",
    "TWENTYNINE", "THIRTY",
];

/// False for depressions/invests that never got a real name (`is_real_storm_name`).
pub fn is_real_storm_name(name: &str) -> bool {
    let n = name.trim().to_uppercase();
    if n.is_empty() || matches!(n.as_str(), "UNNAMED" | "INVEST" | "NONAME") {
        return false;
    }
    if ORDINAL_WORDS.contains(&n.as_str()) {
        return false;
    }
    !n.starts_with("GENESIS")
}

/// Saffir-Simpson category for hurricanes; plain status label otherwise
/// (`category_label`).
fn category_label(status: &str, wind_kt: Option<i64>) -> String {
    if status == "HU" {
        if let Some(w) = wind_kt {
            let cat = match w {
                w if w >= 137 => Some("Category 5"),
                w if w >= 113 => Some("Category 4"),
                w if w >= 96 => Some("Category 3"),
                w if w >= 83 => Some("Category 2"),
                w if w >= 64 => Some("Category 1"),
                _ => None,
            };
            if let Some(c) = cat {
                return c.to_string();
            }
        }
    }
    status_label(status).to_string()
}

fn parse_latlon(lat_raw: &str, lon_raw: &str) -> (f64, f64) {
    let (lat_num, lat_hemi) = lat_raw.split_at(lat_raw.len() - 1);
    let (lon_num, lon_hemi) = lon_raw.split_at(lon_raw.len() - 1);
    let mut lat: f64 = lat_num.parse().unwrap_or(0.0);
    if lat_hemi == "S" {
        lat = -lat;
    }
    let mut lon: f64 = lon_num.parse().unwrap_or(0.0);
    if lon_hemi == "W" {
        lon = -lon;
    }
    (lat, lon)
}

/// One parsed storm: header fields + its track points.
struct ParsedStorm {
    basin: String,
    atcf_id: String,
    year: i64,
    name: String,
    points: Vec<TrackPoint>,
}

/// `parse_hurdat2`: header line + N data lines per storm.
fn parse_hurdat2(text: &str) -> anyhow::Result<Vec<ParsedStorm>> {
    let header_re = regex::Regex::new(r"^([A-Z]{2})(\d{2})(\d{4}),\s*(.+?)\s*,\s*(\d+)\s*,?\s*$")?;
    let lines: Vec<&str> = text.lines().map(|l| l.trim()).filter(|l| !l.is_empty()).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let caps = header_re
            .captures(lines[i])
            .ok_or_else(|| anyhow::anyhow!("Expected a HURDAT2 header line, got: {:?}", lines[i]))?;
        let basin = caps[1].to_string();
        let num = &caps[2];
        let year: i64 = caps[3].parse()?;
        let name = caps[4].to_string();
        let count: usize = caps[5].parse()?;
        let atcf_id = format!("{basin}{num}{year}");

        let mut points = Vec::with_capacity(count);
        for j in 0..count {
            let fields: Vec<String> = lines[i + 1 + j].split(',').map(|f| f.trim().to_string()).collect();
            let (date_raw, time_raw, status) = (&fields[0], &fields[1], &fields[3]);
            let (lat, lon) = parse_latlon(&fields[4], &fields[5]);
            let wind_kt = match fields[6].parse::<i64>() {
                Ok(w) if w >= 0 => Some(w),
                _ => None,
            };
            let pressure_mb = match fields[7].parse::<i64>() {
                Ok(p) if p >= 0 => Some(p),
                _ => None,
            };
            let dt = chrono::NaiveDateTime::parse_from_str(
                &format!("{date_raw}{time_raw}"),
                "%Y%m%d%H%M",
            )?;
            points.push(TrackPoint {
                datetime_utc: dt.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
                status: status.clone(),
                category: category_label(status, wind_kt),
                lat,
                lon,
                wind_kt,
                pressure_mb,
            });
        }
        out.push(ParsedStorm { basin, atcf_id, year, name, points });
        i += 1 + count;
    }
    Ok(out)
}

fn parse_atcf_latlon(raw: &str) -> f64 {
    let (num, hemi) = raw.split_at(raw.len() - 1);
    let mut val: f64 = num.parse::<f64>().unwrap_or(0.0) / 10.0;
    if hemi == "S" || hemi == "W" {
        val = -val;
    }
    val
}

/// `parse_atcf_bdeck`: one b-deck file (all BEST rows for one storm number).
/// Returns None if the storm never got a real name.
fn parse_atcf_bdeck(text: &str, basin: &str, num: &str, year: i64) -> Option<ParsedStorm> {
    use std::collections::BTreeMap;
    let mut by_dt: BTreeMap<String, TrackPoint> = BTreeMap::new();
    let mut final_name = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split(',').map(|f| f.trim()).collect();
        if fields.len() < 28 || fields[4] != "BEST" {
            continue;
        }
        if !fields[27].is_empty() {
            final_name = fields[27].to_string();
        }
        // ATCF timestamps are YYYYMMDDHH (no minutes). chrono needs a complete
        // time, so append "00" for minutes (Python's strptime defaulted them).
        let dt = match chrono::NaiveDateTime::parse_from_str(&format!("{}00", fields[2]), "%Y%m%d%H%M") {
            Ok(d) => d,
            Err(_) => continue,
        };
        let lat = parse_atcf_latlon(fields[6]);
        let lon = parse_atcf_latlon(fields[7]);
        let wind_kt = fields[8].parse::<i64>().ok().filter(|&w| w > 0);
        let pressure_mb = fields[9].parse::<i64>().ok().filter(|&p| p > 0);
        let status = fields[10];
        // Last BEST row for a synoptic hour wins (ATCF convention). BTreeMap key
        // is the ISO datetime so iteration is chronologically sorted.
        by_dt.insert(
            dt.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            TrackPoint {
                datetime_utc: dt.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
                status: status.to_string(),
                category: category_label(status, wind_kt),
                lat,
                lon,
                wind_kt,
                pressure_mb,
            },
        );
    }
    if !is_real_storm_name(&final_name) {
        return None;
    }
    let points: Vec<TrackPoint> = by_dt.into_values().collect();
    if points.is_empty() {
        return None;
    }
    Some(ParsedStorm {
        basin: basin.to_string(),
        atcf_id: format!("{basin}{num}{year}"),
        year,
        name: final_name.to_uppercase(),
        points,
    })
}

/// The newest HURDAT2 file NHC currently publishes for one basin.
struct HurdatFile {
    basin: &'static str,
    filename: String,
    url: String,
    /// The directory listing's "Last modified" column, e.g. "2026-02-27 20:08".
    /// Kept as the raw zero-padded string — it sorts and compares correctly as
    /// text without needing to be parsed into a real datetime.
    last_modified: String,
}

/// Parses NHC's Apache-style directory listing at `HURDAT_INDEX_URL` and picks
/// the most-recently-modified `.txt` file per basin (`discover_latest_hurdat`).
/// NHC keeps every historical reissue of a basin's file online (e.g. three
/// different `hurdat2-1851-2022-*.txt` reprocessings), so filename alone
/// doesn't identify "current" — the listing's own "Last modified" column does.
async fn discover_latest_hurdat(client: &reqwest::Client) -> anyhow::Result<Vec<HurdatFile>> {
    let html = client
        .get(HURDAT_INDEX_URL)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;

    let mut out = Vec::new();
    for (basin, prefix) in HURDAT_BASIN_PREFIXES {
        let (filename, last_modified) = latest_hurdat_from_html(&html, prefix)?
            .ok_or_else(|| anyhow::anyhow!("no hurdat2-{prefix}-*.txt file found in {HURDAT_INDEX_URL} listing"))?;
        out.push(HurdatFile { basin, url: format!("{HURDAT_INDEX_URL}{filename}"), filename, last_modified });
    }
    Ok(out)
}

/// Picks the most-recently-modified `hurdat2-{prefix}-*.txt` entry out of a
/// directory listing's HTML (`(filename, last_modified)`, or `None` if the
/// basin has no matching entry). Split out from `discover_latest_hurdat` so
/// the parsing itself can be unit-tested against a static HTML fixture,
/// without a network call.
fn latest_hurdat_from_html(html: &str, prefix: &str) -> anyhow::Result<Option<(String, String)>> {
    let pattern = format!(
        r#"href="(hurdat2-{prefix}-[^"]+\.txt)">[^<]*</a></td><td align="right">(\d{{4}}-\d{{2}}-\d{{2}} \d{{2}}:\d{{2}})"#
    );
    let re = regex::Regex::new(&pattern)?;
    Ok(re
        .captures_iter(html)
        .map(|c| (c[1].to_string(), c[2].to_string()))
        .max_by(|a, b| a.1.cmp(&b.1)))
}

fn known_hurdat_filename(conn: &Connection, basin: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row("SELECT filename FROM hurdat_sources WHERE basin = ?1", [basin], |r| r.get(0))
        .optional()
}

fn record_hurdat_source(conn: &Connection, file: &HurdatFile) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO hurdat_sources (basin, filename, last_modified, checked_at) VALUES (?1,?2,?3,?4) \
         ON CONFLICT(basin) DO UPDATE SET \
         filename=excluded.filename, last_modified=excluded.last_modified, checked_at=excluded.checked_at",
        rusqlite::params![file.basin, file.filename, file.last_modified, chrono::Utc::now().to_rfc3339()],
    )?;
    Ok(())
}

fn touch_hurdat_checked_at(conn: &Connection, basin: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE hurdat_sources SET checked_at = ?2 WHERE basin = ?1",
        rusqlite::params![basin, chrono::Utc::now().to_rfc3339()],
    )?;
    Ok(())
}

/// Weekly release check (`check_hurdat_updates`): cheap when NOAA hasn't
/// published anything new (one small directory-listing fetch per basin),
/// expensive only when it has. `upsert_storm` is keyed by `atcf_id`
/// (`{basin}{num}{year}` — identical whether the row came from HURDAT2 or
/// ATCF), so re-ingesting a basin's HURDAT2 file here transparently replaces
/// any ATCF-sourced rows for seasons the new release now covers — that's the
/// "prefer HURDAT once it's available" behavior from issue #11.
pub async fn check_hurdat_updates(db_path: &Path) -> anyhow::Result<Value> {
    let conn = get_connection(db_path)?;
    let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(60)).build()?;

    let mut result = serde_json::Map::new();
    for file in discover_latest_hurdat(&client).await? {
        if known_hurdat_filename(&conn, file.basin)?.as_deref() == Some(file.filename.as_str()) {
            touch_hurdat_checked_at(&conn, file.basin)?;
            result.insert(file.basin.to_string(), json!({ "updated": false, "filename": file.filename }));
            continue;
        }
        let text = client.get(&file.url).send().await?.error_for_status()?.text().await?;
        let n = ingest_basin(&conn, &text)?;
        record_hurdat_source(&conn, &file)?;
        result.insert(
            file.basin.to_string(),
            json!({ "updated": true, "filename": file.filename, "storms_ingested": n }),
        );
    }
    Ok(json!({ "hurdat_check": result }))
}

/// Extract b-deck filenames for one basin/year from a directory listing's HTML
/// (`list_atcf_filenames`).
fn list_atcf_filenames(dir_html: &str, basin_prefix: &str, year: i64, gz: bool) -> Vec<String> {
    let suffix = if gz { ".dat.gz" } else { ".dat" };
    let pattern = format!(
        r#"href="(b{}\d{{2}}{}{})""#,
        basin_prefix,
        year,
        regex::escape(suffix)
    );
    let re = match regex::Regex::new(&pattern) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let mut names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for caps in re.captures_iter(dir_html) {
        names.insert(caps[1].to_string());
    }
    names.into_iter().collect()
}

fn upsert_storm(conn: &Connection, s: &ParsedStorm) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO storms (basin, atcf_id, year, name) VALUES (?1,?2,?3,?4) \
         ON CONFLICT(atcf_id) DO UPDATE SET basin=excluded.basin, year=excluded.year, name=excluded.name",
        rusqlite::params![s.basin, s.atcf_id, s.year, s.name],
    )?;
    let storm_id: i64 =
        conn.query_row("SELECT id FROM storms WHERE atcf_id = ?1", [&s.atcf_id], |r| r.get(0))?;
    conn.execute("DELETE FROM track_points WHERE storm_id = ?1", [storm_id])?;
    let mut stmt = conn.prepare(
        "INSERT INTO track_points (storm_id, datetime_utc, status, category, lat, lon, wind_kt, pressure_mb) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
    )?;
    for p in &s.points {
        stmt.execute(rusqlite::params![
            storm_id, p.datetime_utc, p.status, p.category, p.lat, p.lon, p.wind_kt, p.pressure_mb
        ])?;
    }
    Ok(())
}

/// Ingest one HURDAT2 file's named storms; returns the count ingested.
fn ingest_basin(conn: &Connection, text: &str) -> anyhow::Result<i64> {
    let mut count = 0;
    for s in parse_hurdat2(text)? {
        if !is_real_storm_name(&s.name) {
            continue;
        }
        upsert_storm(conn, &s)?;
        count += 1;
    }
    Ok(count)
}

fn max_year_for_basin(conn: &Connection, basin: &str) -> rusqlite::Result<Option<i64>> {
    conn.query_row("SELECT MAX(year) FROM storms WHERE basin = ?1", [basin], |r| {
        r.get::<_, Option<i64>>(0)
    })
}

fn gunzip(bytes: &[u8]) -> anyhow::Result<String> {
    let mut d = flate2::read::GzDecoder::new(bytes);
    let mut s = String::new();
    d.read_to_string(&mut s)?;
    Ok(s)
}

/// Ingest one basin/year of ATCF b-decks (archived+gzipped for a closed season,
/// live+plaintext for the current one). Returns the number of named storms.
async fn ingest_atcf_season(
    client: &reqwest::Client,
    conn: &Connection,
    basin: &str,
    prefix: &str,
    year: i64,
    live: bool,
) -> anyhow::Result<i64> {
    let (dir_url, file_tmpl) = if live {
        (
            "https://ftp.nhc.noaa.gov/atcf/btk/".to_string(),
            "https://ftp.nhc.noaa.gov/atcf/btk/".to_string(),
        )
    } else {
        (
            format!("https://ftp.nhc.noaa.gov/atcf/archive/{year}/"),
            format!("https://ftp.nhc.noaa.gov/atcf/archive/{year}/"),
        )
    };

    let resp = client.get(&dir_url).send().await?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(0);
    }
    let dir_html = resp.error_for_status()?.text().await?;
    let filenames = list_atcf_filenames(&dir_html, prefix, year, !live);

    let mut ingested = 0;
    for filename in filenames {
        let num_start = format!("b{prefix}").len();
        let num = &filename[num_start..num_start + 2];
        let file_url = format!("{file_tmpl}{filename}");
        let r = client.get(&file_url).send().await?;
        if r.status() == reqwest::StatusCode::NOT_FOUND {
            continue;
        }
        let r = r.error_for_status()?;
        let text = if live {
            r.text().await?
        } else {
            gunzip(&r.bytes().await?)?
        };
        if let Some(parsed) = parse_atcf_bdeck(&text, basin, num, year) {
            upsert_storm(conn, &parsed)?;
            ingested += 1;
        }
    }
    Ok(ingested)
}

/// Full HURDAT2 + ATCF ingest pass (`run_ingest`). Returns a JSON summary.
pub async fn run_ingest(db_path: &Path) -> anyhow::Result<Value> {
    let conn = get_connection(db_path)?;
    let current_year = chrono::Utc::now().year() as i64;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?;

    let mut hurdat = serde_json::Map::new();
    for (basin, url) in HURDAT_URLS {
        let text = client.get(url).send().await?.error_for_status()?.text().await?;
        let n = ingest_basin(&conn, &text)?;
        hurdat.insert(basin.to_string(), json!(n));
    }

    let mut atcf = serde_json::Map::new();
    for (basin, prefix) in ATCF_BASINS {
        let start_year = max_year_for_basin(&conn, basin)?.unwrap_or(current_year - 1) + 1;
        let mut basin_summary = serde_json::Map::new();
        for gap_year in start_year..current_year {
            let n = ingest_atcf_season(&client, &conn, basin, prefix, gap_year, false).await?;
            basin_summary.insert(gap_year.to_string(), json!(n));
        }
        let n = ingest_atcf_season(&client, &conn, basin, prefix, current_year, true).await?;
        basin_summary.insert(current_year.to_string(), json!(n));
        atcf.insert(basin.to_string(), Value::Object(basin_summary));
    }

    let total: i64 = conn.query_row("SELECT COUNT(*) FROM storms", [], |r| r.get(0))?;
    Ok(json!({ "hurdat2": hurdat, "atcf": atcf, "total_storms": total }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_storm_name_filter() {
        assert!(is_real_storm_name("LEE"));
        assert!(!is_real_storm_name("UNNAMED"));
        assert!(!is_real_storm_name("invest"));
        assert!(!is_real_storm_name("ONE"));
        assert!(!is_real_storm_name("GENESIS42"));
    }

    #[test]
    fn category_labels() {
        assert_eq!(category_label("HU", Some(140)), "Category 5");
        assert_eq!(category_label("HU", Some(70)), "Category 1");
        assert_eq!(category_label("HU", Some(50)), "HU"); // HU below cat1 + not in label map -> raw status
        assert_eq!(category_label("TS", Some(50)), "Tropical Storm");
        assert_eq!(category_label("TD", None), "Tropical Depression");
    }

    #[test]
    fn atcf_filename_listing() {
        let html = r#"<a href="bal012025.dat.gz">x</a><a href="bal022025.dat.gz">y</a><a href="bep012025.dat.gz">z</a>"#;
        assert_eq!(
            list_atcf_filenames(html, "al", 2025, true),
            vec!["bal012025.dat.gz", "bal022025.dat.gz"]
        );
        // live (non-gz) variant
        let html2 = r#"<a href="bal012026.dat">x</a>"#;
        assert_eq!(list_atcf_filenames(html2, "al", 2026, false), vec!["bal012026.dat"]);
    }

    #[test]
    fn hurdat_listing_picks_newest_by_last_modified_not_filename() {
        // Trimmed fixture of the real Apache autoindex at nhc.noaa.gov/data/hurdat/.
        // Three AL reissues for the same season, deliberately out of filename
        // order, plus an "hurdat2-atl-*" variant and a "-format-" PDF that must
        // NOT match the AL pattern.
        let html = r#"
<tr><td><a href="hurdat2-1851-2022-040723.txt">x</a></td><td align="right">2023-04-07 14:59  </td></tr>
<tr><td><a href="hurdat2-1851-2024-040225.txt">x</a></td><td align="right">2025-04-02 12:00  </td></tr>
<tr><td><a href="hurdat2-1851-2025-02272026.txt">x</a></td><td align="right">2026-02-27 20:08  </td></tr>
<tr><td><a href="hurdat2-atl-1851-2023-042624.txt">x</a></td><td align="right">2024-04-26 09:00  </td></tr>
<tr><td><a href="hurdat2-format-atl-1851-2021.pdf">x</a></td><td align="right">2021-01-01 00:00  </td></tr>
<tr><td><a href="hurdat2-nepac-1949-2025-02272026.txt">x</a></td><td align="right">2026-02-27 20:09  </td></tr>
"#;
        assert_eq!(
            latest_hurdat_from_html(html, "1851").unwrap(),
            Some(("hurdat2-1851-2025-02272026.txt".to_string(), "2026-02-27 20:08".to_string()))
        );
        assert_eq!(
            latest_hurdat_from_html(html, "nepac").unwrap(),
            Some(("hurdat2-nepac-1949-2025-02272026.txt".to_string(), "2026-02-27 20:09".to_string()))
        );
        assert_eq!(latest_hurdat_from_html(html, "cp").unwrap(), None);
    }

    #[test]
    fn hurdat_source_roundtrip_tracks_the_latest_filename() {
        let conn = get_connection(Path::new(":memory:")).unwrap();
        assert_eq!(known_hurdat_filename(&conn, "AL").unwrap(), None);

        let v1 = HurdatFile {
            basin: "AL",
            filename: "hurdat2-1851-2024-040225.txt".to_string(),
            url: "https://example.invalid/hurdat2-1851-2024-040225.txt".to_string(),
            last_modified: "2025-04-02 12:00".to_string(),
        };
        record_hurdat_source(&conn, &v1).unwrap();
        assert_eq!(known_hurdat_filename(&conn, "AL").unwrap(), Some(v1.filename.clone()));

        // A later release for the same basin overwrites the recorded filename.
        let v2 = HurdatFile {
            basin: "AL",
            filename: "hurdat2-1851-2025-02272026.txt".to_string(),
            url: "https://example.invalid/hurdat2-1851-2025-02272026.txt".to_string(),
            last_modified: "2026-02-27 20:08".to_string(),
        };
        record_hurdat_source(&conn, &v2).unwrap();
        assert_eq!(known_hurdat_filename(&conn, "AL").unwrap(), Some(v2.filename));
    }

    #[test]
    fn atcf_bdeck_parses() {
        // A real-format BEST line (name in field 27, timestamp YYYYMMDDHH).
        let line = "AL, 01, 2025062512,   , BEST,   0, 393N,  422W,  30, 1015, LO,   0,    ,    0,    0,    0,    0, 1018,   60,  40,   0,   0,   L,   0,    ,   0,   0,     ANDREA,  ,  0";
        let parsed = parse_atcf_bdeck(line, "AL", "01", 2025).expect("should parse");
        assert_eq!(parsed.name, "ANDREA");
        assert_eq!(parsed.points.len(), 1);
        assert_eq!(parsed.points[0].datetime_utc, "2025-06-25T12:00:00Z");
        assert!((parsed.points[0].lat - 39.3).abs() < 1e-6);
        assert!((parsed.points[0].lon + 42.2).abs() < 1e-6);
    }

    #[test]
    fn hurdat2_parses_a_storm() {
        let text = "AL092023,             LEE,      2,\n\
                    20230905, 0000,  , TD, 13.4N,  38.7W,  30, 1006,\n\
                    20230905, 0600,  , TS, 13.8N,  40.1W,  40, 1004,";
        let storms = parse_hurdat2(text).unwrap();
        assert_eq!(storms.len(), 1);
        let s = &storms[0];
        assert_eq!(s.name, "LEE");
        assert_eq!(s.atcf_id, "AL092023");
        assert_eq!(s.points.len(), 2);
        assert_eq!(s.points[0].datetime_utc, "2023-09-05T00:00:00Z");
        assert_eq!(s.points[0].lat, 13.4);
        assert_eq!(s.points[0].lon, -38.7);
        assert_eq!(s.points[1].wind_kt, Some(40));
    }
}
