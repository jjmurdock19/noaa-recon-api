//! TDR ingest — crawls both source hosts and builds the `tdr.sqlite` index.
//! No manifest exists for either host (same situation as the recon MET
//! archive — see `recon_ingest.rs`), so this walks the directory listings.
//!
//! This module only indexes file *metadata* (mission -> product -> source
//! URL) — it deliberately never downloads a netCDF file. Analysis grids run
//! several MB each and a mission can have a dozen of them; actual
//! download/decompress happens lazily on first request against a cache dir
//! (same pattern as `cache/goes_nc/` in `goes.rs`), not during ingest. That
//! keeps a nightly re-crawl cheap regardless of how large the upstream
//! archive grows.
//!
//! Two hosts, two QC lineages, same file-naming convention:
//! - **Level 1b** (real-time, in-season): flat mission directories at
//!   `seb.omao.noaa.gov/pub/flight/radar/{mission_id}/` — no storm name in
//!   the path.
//! - **Level 2** (post-season, QC'd): `www.aoml.noaa.gov/ftp/pub/hrd/data/
//!   radar/level2/{year}/{storm_slug}/{mission_id}/` — the storm name *is*
//!   part of the path.
//!
//! Only the gridded analysis products are indexed as *files* (`xy`/`xy_rel`,
//! the `vert_inbound`/`vert_outbound` profiles, and the two AWIPS
//! derivatives) — the ancillary `radials.so.gz`/`jobfile.tar.gz` bundles
//! aren't netCDF and aren't what a future slice/passthrough endpoint would
//! ever read, so there's no reason to index them as files. The one
//! exception is `{prefix}_{start}_{stop}_analysis.tar`: its *filename*
//! (never its contents — this module still never downloads a bundle)
//! carries the real HHMM start/stop of the radar leg that produced it, one
//! bundle per leg, which is the only place that boundary actually lives —
//! see `parse_leg_filename` and the `legs` table.
//!
//! Ingest captures TDR's own authoritative storm name: the jobfile name for a
//! Level 1b mission, the path slug for a Level 2 one — see
//! `parse_jobfile_storm`. It's stored directly in `missions.storm_name`, not
//! resolved against any other database. If an admin has corrected a mission's
//! storm identity via the console (`tdr::edit_mission` sets
//! `missions.storm_locked`), a re-crawl must leave `storm_name`/`storm_id`
//! alone — see the `ON CONFLICT` clause in `harvest_mission_dir`.
//!
//! Each analysis also has its own tiny `*_{HHMMSS}_jobfile.tar.gz` (both
//! hosts) — not indexed as a *file*, but every one is fetched once and parsed
//! into the `analyses` table: HRD's "acceptable for composite" verdict and the
//! storm center that analysis's grid was built around (`parse_jobfile_analysis`),
//! which `GET /v1/tdr/composite` uses to filter and align analyses.
//!
//! A mission isn't written off once indexed: its dir keeps being re-listed
//! and diffed against what's on record until it has gone `STABLE_AFTER_SECS`
//! without a new file (the `crawl_state` table), so files published after a
//! partial first crawl still get picked up — see `should_skip_crawl`.

use std::collections::HashSet;
use std::io::Read;
use std::path::Path;
use std::sync::OnceLock;

use chrono::{Datelike, Utc};
use flate2::read::GzDecoder;
use regex::Regex;
use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::services::progress::Progress;
use crate::services::tdr;

const LEVEL1B_BASE: &str = "https://seb.omao.noaa.gov/pub/flight/radar";
const LEVEL2_BASE: &str = "https://www.aoml.noaa.gov/ftp/pub/hrd/data/radar/level2";
const HTTP_TIMEOUT_SECS: u64 = 30;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Level {
    L1b,
    L2,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Level::L1b => "1b",
            Level::L2 => "2",
        }
    }
}

// ── HTTP crawl (same convention as recon_ingest.rs's list_hrefs) ───────────

fn client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent("noaa-recon-api/0.1")
        .timeout(std::time::Duration::from_secs(HTTP_TIMEOUT_SECS))
        .build()?)
}

async fn fetch_bytes(client: &reqwest::Client, url: &str) -> Option<Vec<u8>> {
    match client.get(url).send().await.and_then(|r| r.error_for_status()) {
        Ok(r) => r.bytes().await.ok().map(|b| b.to_vec()),
        Err(e) => {
            tracing::warn!("fetch failed {url}: {e}");
            None
        }
    }
}

async fn list_hrefs(client: &reqwest::Client, url: &str) -> Vec<String> {
    let Some(bytes) = fetch_bytes(client, url).await else {
        return Vec::new();
    };
    let html = String::from_utf8_lossy(&bytes);
    let re = Regex::new(r#"href="([^"]*)""#).unwrap();
    re.captures_iter(&html)
        .map(|c| c[1].to_string())
        .filter(|h| !h.starts_with('?') && h != "/" && !h.starts_with(".."))
        .collect()
}

pub(crate) fn mission_id_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)^\d{8}[hin]\d+[a-z]?$").unwrap())
}

async fn get_level1b_mission_list(client: &reqwest::Client) -> Vec<String> {
    list_hrefs(client, &format!("{LEVEL1B_BASE}/"))
        .await
        .into_iter()
        .map(|h| h.trim_end_matches('/').to_string())
        .filter(|h| mission_id_re().is_match(h))
        .collect()
}

async fn get_level2_storm_slugs(client: &reqwest::Client, year: i64) -> Vec<String> {
    let re = Regex::new(r"^[A-Za-z0-9]+$").unwrap();
    list_hrefs(client, &format!("{LEVEL2_BASE}/{year}/"))
        .await
        .into_iter()
        .map(|h| h.trim_end_matches('/').to_string())
        .filter(|h| re.is_match(h))
        .collect()
}

async fn get_level2_mission_list(client: &reqwest::Client, year: i64, slug: &str) -> Vec<String> {
    list_hrefs(client, &format!("{LEVEL2_BASE}/{year}/{slug}/"))
        .await
        .into_iter()
        .map(|h| h.trim_end_matches('/').to_string())
        .filter(|h| mission_id_re().is_match(h))
        .collect()
}

// ── Filename parsing ────────────────────────────────────────────────────────

struct ParsedFile {
    /// The exact variant tag (`xy`, `xy_rel`, `vert_inbound`, `vert_inbound_rel`,
    /// `vert_inbound_fall`, `vert_outbound`, `vert_outbound_rel`,
    /// `vert_outbound_fall`, `awips_maxdb`, `awips_wind`) — this, not just
    /// `storm_relative`/`fall_speed_removed`, is what the DB's UNIQUE
    /// constraint discriminates on, since a mission can have the plain,
    /// `_rel`, and `_fall` variants of the same product at the same
    /// analysis time as three genuinely separate files.
    product: String,
    format: &'static str,
    analysis_time: String,
    storm_relative: bool,
    fall_speed_removed: bool,
}

/// Recognizes the gridded analysis products: `{YYMMDDAI}_{HHMM}_xy(_rel).nc(.gz)`,
/// `{YYMMDDAI}_{HHMM}_vert_in(out)bound(_rel|_fall).nc(.gz)`, and the two AWIPS
/// derivatives. Everything else (execution logs, superobs, jobfiles) is
/// deliberately ignored — see the module doc comment.
fn parse_product_filename(name: &str) -> Option<ParsedFile> {
    static XY_VERT_RE: OnceLock<Regex> = OnceLock::new();
    let re = XY_VERT_RE.get_or_init(|| {
        Regex::new(
            r"(?i)^\d{6}[a-z]\d+_(\d{4})_((?:xy(?:_rel)?)|(?:vert_(?:in|out)bound(?:_(?:rel|fall))?))\.(nc|w)(?:\.gz)?$",
        )
        .unwrap()
    });
    if let Some(c) = re.captures(name) {
        let analysis_time = c[1].to_string();
        let product = c[2].to_lowercase();
        let format = if c[3].eq_ignore_ascii_case("nc") { "nc" } else { "w" };
        let storm_relative = product.ends_with("_rel");
        let fall_speed_removed = product.ends_with("_fall");
        return Some(ParsedFile { product, format, analysis_time, storm_relative, fall_speed_removed });
    }

    static AWIPS_RE: OnceLock<Regex> = OnceLock::new();
    let re2 = AWIPS_RE.get_or_init(|| {
        Regex::new(r"(?i)^AWIPS(Maxdb|WindComponents)_\d{6}[a-z]\d+_(\d{4})z\.nc(?:\.gz)?$").unwrap()
    });
    let c = re2.captures(name)?;
    let product = if c[1].eq_ignore_ascii_case("maxdb") { "awips_maxdb" } else { "awips_wind" }.to_string();
    Some(ParsedFile {
        product,
        format: "nc",
        analysis_time: c[2].to_string(),
        storm_relative: false,
        fall_speed_removed: false,
    })
}

/// Recognizes a leg's bundle filename, `{YYMMDDAI}_{startHHMM}_{stopHHMM}_analysis.tar`
/// (optionally `.gz`) — confirmed against a live crawl of both hosts to be
/// present for every leg, one bundle each, with the two 4-digit groups being
/// that leg's actual radar-on/radar-off times (not analysis_times — those
/// only mark when one product inside the leg was centered). Returns
/// `(start_time, stop_time)`. Only the filename is ever read; the tar itself
/// is never fetched — see the module doc comment.
fn parse_leg_filename(name: &str) -> Option<(String, String)> {
    static LEG_RE: OnceLock<Regex> = OnceLock::new();
    let re = LEG_RE
        .get_or_init(|| Regex::new(r"(?i)^\d{6}[a-z]\d+_(\d{4})_(\d{4})_analysis\.tar(?:\.gz)?$").unwrap());
    let c = re.captures(name)?;
    Some((c[1].to_string(), c[2].to_string()))
}

pub(crate) fn mission_year(mission_id: &str) -> Option<i64> {
    mission_id.get(0..4)?.parse().ok()
}

/// `N42/3/9 = H/I/N` per the AOML TDR README's filename convention.
fn aircraft_from_mission_id(mission_id: &str) -> (Option<String>, Option<String>) {
    match mission_id.as_bytes().get(8).map(|b| b.to_ascii_uppercase()) {
        Some(b'H') => (Some("NOAA 42 (Kermit)".into()), Some("N42".into())),
        Some(b'I') => (Some("NOAA 43 (Miss Piggy)".into()), Some("N43".into())),
        Some(b'N') => (Some("NOAA 49 (Gonzo)".into()), Some("N49".into())),
        _ => (None, None),
    }
}

/// Python `str.title()` for storm slugs/names.
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

/// Every per-analysis `*_jobfile.tar.gz` in a mission dir listing, sorted by
/// filename. Level 1b names them `{processedYYYYMMDDHHMMSS}_{mission}_
/// {centerHHMMSS}_jobfile.tar.gz`, so sorting puts a re-run of the same
/// analysis *after* the original and its upsert wins; Level 2 names them
/// `{mission}_{centerHHMMSS}_jobfile.tar.gz` (one per analysis).
fn jobfile_names(hrefs: &[String]) -> Vec<String> {
    let mut names: Vec<String> = hrefs
        .iter()
        .map(|h| h.rsplit('/').next().unwrap_or(h).to_string())
        .filter(|n| n.to_lowercase().ends_with("_jobfile.tar.gz"))
        .collect();
    names.sort();
    names.dedup();
    names
}

/// The `{centerHHMMSS}` suffix of a jobfile's name — the same on both hosts.
fn jobfile_center_time(name: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"(?i)_(\d{6})_jobfile\.tar\.gz$").unwrap());
    Some(re.captures(name)?[1].to_string())
}

/// Fetches one `*_jobfile.tar.gz` and returns its gunzipped text. The gzip
/// wraps a tiny (~1.6 KB) tar whose `jobfile.xml` member is a single line like
/// `<flight id="20251030H1" mission="3113A MELISSA" storm="AL132025" …>` and
/// whose `summary` member is a short plaintext recap. Because tar stores member
/// *contents* uncompressed, the gunzipped bytes carry both verbatim — so we
/// gunzip and regex them straight out rather than pull in a tar reader for one
/// 1.6 KB blob. The jobfile is the ONLY in-directory source of the storm name
/// for a Level 1b mission (no plaintext index exists), which is exactly what
/// lets a radar-only mission that landed before its recon MET data get a real
/// (if still provisional) name instead of "Unknown" — and the only source of
/// each analysis's composite-acceptability verdict and center.
async fn fetch_jobfile_text(http: &reqwest::Client, url: &str) -> Option<String> {
    let gz = fetch_bytes(http, url).await?;
    // The tar's binary headers aren't valid UTF-8, so read as bytes then
    // lossy-decode — the `<flight …>` line and summary are plain ASCII regardless.
    let mut buf = Vec::new();
    GzDecoder::new(&gz[..]).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// One analysis's metadata from its jobfile — see the `analyses` table in
/// `tdr::SCHEMA`.
#[derive(Debug, PartialEq)]
struct JobfileAnalysis {
    /// HHMM — the `files.analysis_time` key of the products this jobfile built.
    analysis_time: String,
    /// Full HHMMSS `centerTime`.
    center_time: String,
    center_lat: Option<f64>,
    center_lon: Option<f64>,
    storm_dir_deg: Option<f64>,
    storm_speed_kt: Option<f64>,
    acceptable_for_composite: Option<bool>,
}

/// Pulls one analysis's metadata out of a gunzipped jobfile tar's text.
/// Split out for unit testing, same as `parse_jobfile_storm`.
///
/// - `analysis_time`: the XML `<centerTime>` (HHMMSS), truncated to HHMM —
///   confirmed against live dirs (centerTime `134908` built `…_1349_xy.nc`).
///   Falls back to the `_{HHMMSS}_jobfile` filename suffix if the XML has none.
/// - `acceptable_for_composite`: XML `<acceptable>` (0/1), else the summary's
///   `Acceptable for composite: N` line.
/// - center: the summary's signed `Center lat, lon:` line first (no
///   hemisphere guessing), else XML `latDeg`/`lonDeg` (+ minutes), with
///   `lonHemisphere` 0 meaning west — the only encoding seen on either host,
///   cross-checked against the signed summary line in both.
fn parse_jobfile_analysis(text: &str, filename: &str) -> Option<JobfileAnalysis> {
    static TAG_RE: OnceLock<Regex> = OnceLock::new();
    static SUMMARY_CENTER_RE: OnceLock<Regex> = OnceLock::new();
    static SUMMARY_ACCEPT_RE: OnceLock<Regex> = OnceLock::new();
    let tag_re = TAG_RE.get_or_init(|| Regex::new(r"<([A-Za-z0-9]+)>\s*([^<]*?)\s*</[A-Za-z0-9]+>").unwrap());
    let tag = |name: &str| -> Option<&str> {
        tag_re.captures_iter(text).find(|c| &c[1] == name).map(|c| c.get(2).unwrap().as_str())
    };
    let tag_f64 = |name: &str| tag(name).and_then(|v| v.parse::<f64>().ok());

    let center_time = match tag("centerTime").filter(|t| t.len() == 6 && t.bytes().all(|b| b.is_ascii_digit())) {
        Some(t) => t.to_string(),
        None => jobfile_center_time(filename)?,
    };
    let analysis_time = center_time[..4].to_string();

    let accept_re =
        SUMMARY_ACCEPT_RE.get_or_init(|| Regex::new(r"(?i)Acceptable for composite:\s*(\d)").unwrap());
    let acceptable_for_composite = tag("acceptable")
        .and_then(|v| v.parse::<i64>().ok())
        .or_else(|| accept_re.captures(text).and_then(|c| c[1].parse().ok()))
        .map(|v| v != 0);

    let center_re = SUMMARY_CENTER_RE
        .get_or_init(|| Regex::new(r"(?i)Center lat, lon:\s*(-?\d+(?:\.\d+)?),\s*(-?\d+(?:\.\d+)?)").unwrap());
    let (center_lat, center_lon) = match center_re.captures(text) {
        Some(c) => (c[1].parse().ok(), c[2].parse().ok()),
        None => {
            let deg_min = |d: &str, m: &str| Some(tag_f64(d)? + tag_f64(m).unwrap_or(0.0) / 60.0);
            let lat = deg_min("latDeg", "latMin");
            let lon = deg_min("lonDeg", "lonMin").map(|lon| {
                if tag("lonHemisphere") == Some("0") { -lon.abs() } else { lon }
            });
            (lat, lon)
        }
    };
    // A 0/0 center is the jobfile's "unset", not a real fix.
    let (center_lat, center_lon) = match (center_lat, center_lon) {
        (Some(la), Some(lo)) if !(la == 0.0 && lo == 0.0) => (Some(la), Some(lo)),
        _ => (None, None),
    };

    Some(JobfileAnalysis {
        analysis_time,
        center_time,
        center_lat,
        center_lon,
        storm_dir_deg: tag_f64("stmDir"),
        storm_speed_kt: tag_f64("stmMotion"),
        acceptable_for_composite,
    })
}

/// The flight-number token used for a weather-reconnaissance training
/// flight — e.g. `mission="WXWXA ET02"` — as opposed to a real storm
/// mission's numeric flight number (`mission="0106E FAUSTO"`). Unlike a
/// storm mission, the token *after* `WXWXA` is an exercise id, not a name,
/// so it must be special-cased before the normal "second token is the name"
/// parse below ever sees it.
const TRAINING_FLIGHT_TOKEN: &str = "WXWXA";
const TRAINING_STORM_NAME: &str = "Training";

/// Pulls `(storm_name, atcf_id)` from a gunzipped jobfile tar's text (the
/// `<flight mission="…" storm="…">` line). Split out for unit testing.
fn parse_jobfile_storm(text: &str) -> Option<(String, Option<String>)> {
    static MISSION_RE: OnceLock<Regex> = OnceLock::new();
    static STORM_RE: OnceLock<Regex> = OnceLock::new();
    let mission_re = MISSION_RE.get_or_init(|| Regex::new(r#"mission="([^"]*)""#).unwrap());
    let storm_re = STORM_RE.get_or_init(|| Regex::new(r#"storm="([A-Za-z]{2}\d{6})""#).unwrap());

    let mission_val = mission_re.captures(text)?.get(1)?.as_str().trim().to_string();
    // "3113A MELISSA" → the name is everything after the leading flight-number
    // token. A training flight's leading token is WXWXA (not a flight
    // number) followed by an exercise id, not a storm name — e.g.
    // "WXWXA ET02" — so it's labeled outright rather than misread as a
    // storm called "Et02". A plain ferry/training jobfile with no second
    // token at all still has no name → skip.
    let mut parts = mission_val.split_whitespace();
    let flight_num = parts.next()?;
    if flight_num.eq_ignore_ascii_case(TRAINING_FLIGHT_TOKEN) {
        return Some((TRAINING_STORM_NAME.to_string(), None));
    }
    let name_raw = parts.collect::<Vec<_>>().join(" ");
    if name_raw.is_empty() {
        return None;
    }
    let name = title_case(&name_raw);
    if name.len() < 2 || is_junk_storm_name(&name) {
        return None;
    }
    let atcf = storm_re.captures(text).map(|c| c[1].to_uppercase());
    Some((name, atcf))
}

fn is_junk_storm_name(name: &str) -> bool {
    matches!(
        name.to_uppercase().as_str(),
        "TEST" | "NONE" | "N/A" | "UNKNOWN" | "FERRY" | "TRAINING" | "INVEST" | "SURVEY" | "RECON" | "CYCLONE"
    )
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Upserts one mission's own columns. Split out from `harvest_mission_dir` for
/// unit testing (no HTTP involved) — same reasoning as `parse_jobfile_storm`.
/// A fresh insert always starts unlocked (`storm_locked = 0`). On a re-crawl:
/// if an admin has locked the row (a manual correction via the console),
/// `storm_name`/`storm_id` are left alone entirely; otherwise COALESCE
/// prefers the incoming value but falls back to what's already stored, so a
/// later label-less pass (e.g. a training-dir jobfile) can't wipe out a good
/// name.
#[allow(clippy::too_many_arguments)]
fn upsert_mission(
    conn: &Connection,
    mission_id: &str,
    year: i64,
    aircraft: Option<&str>,
    tail_num: Option<&str>,
    storm_name: Option<&str>,
    storm_id: Option<&str>,
    level1b_flag: i64,
    level2_flag: i64,
    fetched_at: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO missions \
         (mission_id, year, aircraft, tail_num, storm_name, storm_id, storm_locked, has_level1b, has_level2, \
          fetched_at) \
         VALUES (?1,?2,?3,?4,?5,?6,0,?7,?8,?9) \
         ON CONFLICT(mission_id) DO UPDATE SET \
           aircraft=excluded.aircraft, tail_num=excluded.tail_num, \
           storm_name=CASE WHEN missions.storm_locked=1 THEN missions.storm_name \
                            ELSE COALESCE(excluded.storm_name, missions.storm_name) END, \
           storm_id=CASE WHEN missions.storm_locked=1 THEN missions.storm_id \
                          ELSE COALESCE(excluded.storm_id, missions.storm_id) END, \
           has_level1b=MAX(missions.has_level1b, excluded.has_level1b), \
           has_level2=MAX(missions.has_level2, excluded.has_level2), \
           fetched_at=excluded.fetched_at",
        rusqlite::params![
            mission_id, year, aircraft, tail_num, storm_name, storm_id,
            level1b_flag, level2_flag, fetched_at,
        ],
    )?;
    Ok(())
}

/// How long a mission dir keeps being re-listed after the last time a crawl
/// found something new in it. A Level 1b mission is published file-by-file
/// while (and shortly after) the aircraft flies, and Level 2 is uploaded in
/// batches post-season, so "this level is already indexed" can't mean "this
/// mission is done" — only a quiet stretch with no new files can.
const STABLE_AFTER_SECS: i64 = 14 * 24 * 3600;

/// Ingest's skip rule, split out for unit testing: skip re-listing a mission
/// dir only when this level is already indexed *and* has a `crawl_state`
/// row whose last change is older than [`STABLE_AFTER_SECS`]. No
/// `crawl_state` row means the mission predates that table (or was never
/// fully crawled), so it gets one re-list — which is also what backfills
/// the `analyses` table for missions indexed before it existed.
fn should_skip_crawl(conn: &Connection, mission_id: &str, level: Level, force: bool, now: i64) -> bool {
    if force {
        return false;
    }
    let already: i64 = conn
        .query_row(
            match level {
                Level::L1b => "SELECT has_level1b FROM missions WHERE mission_id = ?1",
                Level::L2 => "SELECT has_level2 FROM missions WHERE mission_id = ?1",
            },
            [mission_id],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if already == 0 {
        return false;
    }
    let last_changed: Option<i64> = conn
        .query_row(
            "SELECT last_changed_at FROM crawl_state WHERE mission_id = ?1 AND level = ?2",
            rusqlite::params![mission_id, level.as_str()],
            |r| r.get(0),
        )
        .optional()
        .unwrap_or(None);
    last_changed.is_some_and(|t| now - t > STABLE_AFTER_SECS)
}

/// Every source URL already on record for one (mission, level) — files,
/// legs, and jobfiles — so a re-list can tell what's genuinely new.
fn known_source_urls(conn: &Connection, mission_id: &str, level: Level) -> rusqlite::Result<HashSet<String>> {
    let mut known = HashSet::new();
    for table in ["files", "legs", "analyses"] {
        let mut stmt = conn.prepare(&format!("SELECT source_url FROM {table} WHERE mission_id = ?1 AND level = ?2"))?;
        let rows = stmt.query_map(rusqlite::params![mission_id, level.as_str()], |r| r.get::<_, String>(0))?;
        for url in rows {
            known.insert(url?);
        }
    }
    Ok(known)
}

/// `analysis_time -> jobfile source_url` currently on record for one
/// (mission, level).
fn stored_jobfile_urls(
    conn: &Connection,
    mission_id: &str,
    level: Level,
) -> rusqlite::Result<std::collections::HashMap<String, String>> {
    let mut stmt = conn.prepare("SELECT analysis_time, source_url FROM analyses WHERE mission_id = ?1 AND level = ?2")?;
    let rows = stmt.query_map(rusqlite::params![mission_id, level.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))?;
    rows.collect()
}

fn record_crawl(conn: &Connection, mission_id: &str, level: Level, now: i64, changed: bool) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO crawl_state (mission_id, level, last_crawled_at, last_changed_at) VALUES (?1, ?2, ?3, ?3) \
         ON CONFLICT(mission_id, level) DO UPDATE SET \
           last_crawled_at = excluded.last_crawled_at, \
           last_changed_at = CASE WHEN ?4 THEN excluded.last_changed_at ELSE crawl_state.last_changed_at END",
        rusqlite::params![mission_id, level.as_str(), now, changed],
    )?;
    Ok(())
}

// ── Per-mission harvest ──────────────────────────────────────────────────────

/// Crawls one mission directory's file listing and upserts whatever's new in
/// it — products, legs, and per-analysis jobfiles. Returns whether anything
/// new was indexed. See [`should_skip_crawl`] for when the listing isn't
/// even fetched; otherwise the listing is always diffed against what's on
/// record, so a mission first indexed mid-flight still picks up files that
/// land later instead of being written off as "already ingested".
#[allow(clippy::too_many_arguments)]
async fn harvest_mission_dir(
    http: &reqwest::Client,
    conn: &Connection,
    mission_id: &str,
    year: i64,
    level: Level,
    mission_url: &str,
    level2_storm_slug: Option<&str>,
    force: bool,
) -> anyhow::Result<bool> {
    let now = now_unix();
    if should_skip_crawl(conn, mission_id, level, force, now) {
        return Ok(false);
    }

    let hrefs = list_hrefs(http, mission_url).await;
    let files: Vec<(ParsedFile, String)> = hrefs
        .iter()
        .filter_map(|h| {
            let name = h.rsplit('/').next().unwrap_or(h);
            parse_product_filename(name).map(|f| (f, format!("{mission_url}{name}")))
        })
        .collect();
    if files.is_empty() {
        return Ok(false);
    }
    let legs: Vec<(String, String, String)> = hrefs
        .iter()
        .filter_map(|h| {
            let name = h.rsplit('/').next().unwrap_or(h);
            parse_leg_filename(name).map(|(start, stop)| (start, stop, format!("{mission_url}{name}")))
        })
        .collect();
    let jobfile_urls: Vec<(String, String)> =
        jobfile_names(&hrefs).into_iter().map(|n| (format!("{mission_url}{n}"), n)).collect();

    // Diff the listing against what's on record. With nothing new (and no
    // `force`), there's nothing to write beyond noting the crawl happened.
    let known = known_source_urls(conn, mission_id, level)?;
    let is_new = |url: &String| force || !known.contains(url);
    // An analysis re-run leaves both jobfiles in the dir but only the later
    // one's URL on record (it overwrote the row), so a jobfile also counts
    // as seen when the stored row for its analysis time came from a jobfile
    // that sorts at or after it — see `jobfile_names`.
    let stored_jobfiles = stored_jobfile_urls(conn, mission_id, level)?;
    let is_new_jobfile = |url: &String, name: &str| {
        is_new(url)
            && !jobfile_center_time(name)
                .and_then(|t| stored_jobfiles.get(&t[..4]))
                .is_some_and(|stored| !force && stored.as_str() >= url.as_str())
    };
    let any_new = files.iter().any(|(_, u)| is_new(u))
        || legs.iter().any(|(_, _, u)| is_new(u))
        || jobfile_urls.iter().any(|(u, n)| is_new_jobfile(u, n));
    let mission_exists = conn
        .query_row("SELECT 1 FROM missions WHERE mission_id = ?1", [mission_id], |_| Ok(()))
        .optional()?
        .is_some();
    if !any_new && mission_exists {
        record_crawl(conn, mission_id, level, now, false)?;
        return Ok(false);
    }

    // Only fetch jobfiles we haven't parsed before (all of them on `force`).
    // Each yields one analysis's composite verdict + center; the first one
    // that names a storm also supplies TDR's own authoritative storm name
    // (+ ATCF) for a Level 1b mission — Level 2 names it in the path slug.
    let mut analyses: Vec<(JobfileAnalysis, String)> = Vec::new();
    let mut jobfile_storm: Option<(String, Option<String>)> = None;
    for (url, name) in jobfile_urls.iter().filter(|(u, n)| is_new_jobfile(u, n)) {
        let Some(text) = fetch_jobfile_text(http, url).await else { continue };
        if jobfile_storm.is_none() {
            jobfile_storm = parse_jobfile_storm(&text);
        }
        match parse_jobfile_analysis(&text, name) {
            Some(a) => analyses.push((a, url.clone())),
            None => tracing::warn!("{mission_id} ({}): couldn't parse jobfile {name}", level.as_str()),
        }
    }
    // A training/ferry dir with no name, or a re-list that fetched no new
    // jobfile, leaves `storm_name` NULL here — the upsert's COALESCE keeps
    // whatever's already stored.
    let (storm_name, storm_id): (Option<String>, Option<String>) = match level {
        Level::L2 => (level2_storm_slug.map(title_case), None),
        Level::L1b => match jobfile_storm {
            Some((name, atcf)) => (Some(name), atcf),
            None => (None, None),
        },
    };
    let (aircraft, tail_num) = aircraft_from_mission_id(mission_id);
    let fetched_at = now;

    conn.execute_batch("BEGIN")?;
    let res = (|| -> rusqlite::Result<()> {
        let (level1b_flag, level2_flag) = match level {
            Level::L1b => (1, 0),
            Level::L2 => (0, 1),
        };
        upsert_mission(
            conn, mission_id, year, aircraft.as_deref(), tail_num.as_deref(),
            storm_name.as_deref(), storm_id.as_deref(), level1b_flag, level2_flag, fetched_at,
        )?;

        let mut stmt = conn.prepare(
            "INSERT INTO files \
             (mission_id, level, product, format, analysis_time, storm_relative, fall_speed_removed, source_url, fetched_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9) \
             ON CONFLICT(mission_id, level, product, format, analysis_time) DO UPDATE SET \
               storm_relative=excluded.storm_relative, fall_speed_removed=excluded.fall_speed_removed, \
               source_url=excluded.source_url, fetched_at=excluded.fetched_at",
        )?;
        for (f, url) in &files {
            stmt.execute(rusqlite::params![
                mission_id,
                level.as_str(),
                f.product,
                f.format,
                f.analysis_time,
                f.storm_relative as i64,
                f.fall_speed_removed as i64,
                url,
                fetched_at,
            ])?;
        }

        let mut leg_stmt = conn.prepare(
            "INSERT INTO legs (mission_id, level, start_time, stop_time, source_url, fetched_at) \
             VALUES (?1,?2,?3,?4,?5,?6) \
             ON CONFLICT(mission_id, level, start_time, stop_time) DO UPDATE SET \
               source_url=excluded.source_url, fetched_at=excluded.fetched_at",
        )?;
        for (start, stop, url) in &legs {
            leg_stmt.execute(rusqlite::params![mission_id, level.as_str(), start, stop, url, fetched_at])?;
        }

        for (a, url) in &analyses {
            upsert_analysis(conn, mission_id, level, a, url, fetched_at)?;
        }
        record_crawl(conn, mission_id, level, now, true)?;
        Ok(())
    })();
    match res {
        Ok(()) => {
            conn.execute_batch("COMMIT")?;
            tracing::info!(
                "{mission_id} ({}): indexed {} product file(s), {} leg(s), {} new jobfile analysis(es)",
                level.as_str(),
                files.len(),
                legs.len(),
                analyses.len()
            );
            Ok(true)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e.into())
        }
    }
}

/// Upserts one analysis's jobfile metadata. Split out for unit testing.
fn upsert_analysis(
    conn: &Connection,
    mission_id: &str,
    level: Level,
    a: &JobfileAnalysis,
    source_url: &str,
    fetched_at: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO analyses \
         (mission_id, level, analysis_time, center_time, center_lat, center_lon, storm_dir_deg, storm_speed_kt, \
          acceptable_for_composite, source_url, fetched_at) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11) \
         ON CONFLICT(mission_id, level, analysis_time) DO UPDATE SET \
           center_time=excluded.center_time, center_lat=excluded.center_lat, center_lon=excluded.center_lon, \
           storm_dir_deg=excluded.storm_dir_deg, storm_speed_kt=excluded.storm_speed_kt, \
           acceptable_for_composite=excluded.acceptable_for_composite, \
           source_url=excluded.source_url, fetched_at=excluded.fetched_at",
        rusqlite::params![
            mission_id,
            level.as_str(),
            a.analysis_time,
            a.center_time,
            a.center_lat,
            a.center_lon,
            a.storm_dir_deg,
            a.storm_speed_kt,
            a.acceptable_for_composite.map(|v| v as i64),
            source_url,
            fetched_at,
        ],
    )?;
    Ok(())
}

// ── Orchestration ────────────────────────────────────────────────────────────

/// Full TDR ingest (`run_ingest`). `years` defaults to [current-1, current],
/// same as the recon MET archive. Crawls Level 1b (flat mission dirs, all
/// years live under one listing so the `years` filter is applied after the
/// fact) and Level 2 (year -> storm -> mission) for each requested year.
///
/// `progress` carries the live phase/counter the admin console polls; CLI and
/// timer callers pass a `Progress::default()` and ignore it.
pub async fn run_ingest(
    tdr_db: &Path,
    years: Option<Vec<i64>>,
    force: bool,
    progress: &Progress,
) -> anyhow::Result<Value> {
    let years = years.unwrap_or_else(|| {
        let y = Utc::now().year() as i64;
        vec![y - 1, y]
    });

    // WRITE connection: applies/migrates the schema. Storm identity is TDR's
    // own now, so ingest never opens any other database.
    let conn = tdr::init_db(tdr_db)?;
    let http = client()?;

    let (mut ingested_1b, mut ingested_2, mut skipped, mut errors) = (0i64, 0i64, 0i64, 0i64);

    progress.phase("Level 1b missions", None);
    // All years share one flat listing, so the year filter runs after the
    // crawl — the total counts every mission listed, not just this year's.
    let level1b = get_level1b_mission_list(&http).await;
    progress.set_total(level1b.len() as i64);
    for mission_id in level1b {
        let Some(year) = mission_year(&mission_id) else {
            progress.step();
            continue;
        };
        if !years.contains(&year) {
            progress.step();
            continue;
        }
        let mission_url = format!("{LEVEL1B_BASE}/{mission_id}/");
        progress.detail(&mission_id);
        match harvest_mission_dir(&http, &conn, &mission_id, year, Level::L1b, &mission_url, None, force)
            .await
        {
            Ok(true) => ingested_1b += 1,
            Ok(false) => skipped += 1,
            Err(e) => {
                tracing::error!("{mission_id} (Level 1b): {e}");
                errors += 1;
            }
        }
        progress.step();
    }

    for year in &years {
        progress.phase(format!("Level 2 storms {year}"), None);
        let slugs = get_level2_storm_slugs(&http, *year).await;
        progress.set_total(slugs.len() as i64);
        for slug in slugs {
            progress.detail(&slug);
            for mission_id in get_level2_mission_list(&http, *year, &slug).await {
                let mission_url = format!("{LEVEL2_BASE}/{year}/{slug}/{mission_id}/");
                match harvest_mission_dir(
                    &http,
                    &conn,
                    &mission_id,
                    *year,
                    Level::L2,
                    &mission_url,
                    Some(&slug),
                    force,
                )
                .await
                {
                    Ok(true) => ingested_2 += 1,
                    Ok(false) => skipped += 1,
                    Err(e) => {
                        tracing::error!("{mission_id} (Level 2): {e}");
                        errors += 1;
                    }
                }
            }
            progress.step();
        }
    }

    progress.phase("Counting", None);
    let total_missions: i64 = conn.query_row("SELECT COUNT(*) FROM missions", [], |r| r.get(0))?;
    let total_files: i64 = conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))?;

    Ok(json!({
        "years": years,
        "ingested_level1b": ingested_1b,
        "ingested_level2": ingested_2,
        "skipped": skipped,
        "errors": errors,
        "total_missions": total_missions,
        "total_files": total_files,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(tdr::SCHEMA).unwrap();
        conn
    }

    #[test]
    fn re_crawl_updates_an_unlocked_mission() {
        let conn = mem_conn();
        upsert_mission(&conn, "20260616H1", 2026, Some("N42"), Some("N42"), Some("Fausto"), Some("EP062026"), 1, 0, 100).unwrap();
        // Simulated re-crawl with a different (e.g. re-parsed) name.
        upsert_mission(&conn, "20260616H1", 2026, Some("N42"), Some("N42"), Some("Fausto2"), Some("EP062026"), 1, 0, 200).unwrap();

        let name: String = conn.query_row("SELECT storm_name FROM missions", [], |r| r.get(0)).unwrap();
        assert_eq!(name, "Fausto2");
    }

    #[test]
    fn re_crawl_never_touches_a_locked_mission() {
        let conn = mem_conn();
        upsert_mission(&conn, "20260616H1", 2026, Some("N42"), Some("N42"), Some("Training / Research"), None, 1, 0, 100).unwrap();
        // An admin correction, mirroring what tdr::edit_mission does.
        conn.execute(
            "UPDATE missions SET storm_name = 'Fausto', storm_id = 'EP062026', storm_locked = 1 WHERE mission_id = '20260616H1'",
            [],
        )
        .unwrap();

        // A re-crawl that would otherwise demote it right back.
        upsert_mission(&conn, "20260616H1", 2026, Some("N42"), Some("N42"), Some("Training / Research"), None, 1, 0, 200).unwrap();

        let (name, id): (String, Option<String>) =
            conn.query_row("SELECT storm_name, storm_id FROM missions", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!(name, "Fausto", "locked mission must survive a re-crawl unchanged");
        assert_eq!(id.as_deref(), Some("EP062026"));
    }

    #[test]
    fn parses_storm_from_jobfile_xml() {
        let text = r#"<flight id="20251030H1" mission="3113A MELISSA" storm="AL132025" mode="0"><start3D>174741</start3D></flight>"#;
        let (name, atcf) = parse_jobfile_storm(text).unwrap();
        assert_eq!(name, "Melissa");
        assert_eq!(atcf.as_deref(), Some("AL132025"));
    }

    #[test]
    fn parses_storm_name_from_second_token() {
        let text = r#"<flight id="20260616H1" mission="0106E FAUSTO" storm="EP062026">"#;
        let (name, atcf) = parse_jobfile_storm(text).unwrap();
        assert_eq!(name, "Fausto");
        assert_eq!(atcf.as_deref(), Some("EP062026"));
    }

    #[test]
    fn wxwxa_leading_token_is_labeled_training() {
        // The exercise id after WXWXA (here "ET02") is not a storm name —
        // must not be mistaken for one, regardless of what it looks like.
        let text = r#"<flight id="20260101H1" mission="WXWXA ET02" storm="">"#;
        let (name, atcf) = parse_jobfile_storm(text).unwrap();
        assert_eq!(name, "Training");
        assert_eq!(atcf, None);

        let lower = r#"<flight mission="wxwxa RANDOMTOKEN">"#;
        assert_eq!(parse_jobfile_storm(lower).unwrap().0, "Training");
    }

    #[test]
    fn jobfile_without_storm_name_is_none() {
        // ferry/training jobfile: mission is just the flight number, no name.
        assert!(parse_jobfile_storm(r#"<flight id="20250101H1" mission="0101A" storm="">"#).is_none());
        assert!(parse_jobfile_storm(r#"<flight mission="0101A FERRY">"#).is_none());
        assert!(parse_jobfile_storm("no flight element here").is_none());
    }

    #[test]
    fn parses_xy_and_vert_products() {
        let f = parse_product_filename("240630I1_1201_xy.nc.gz").unwrap();
        assert_eq!(f.product, "xy");
        assert_eq!(f.format, "nc");
        assert_eq!(f.analysis_time, "1201");
        assert!(!f.storm_relative);
        assert!(!f.fall_speed_removed);

        let f = parse_product_filename("240630I1_1324_vert_inbound_fall.w.gz").unwrap();
        assert_eq!(f.product, "vert_inbound_fall");
        assert_eq!(f.format, "w");
        assert!(f.fall_speed_removed);
        assert!(!f.storm_relative);

        let f = parse_product_filename("200913N1_1201_vert_outbound_rel.nc.gz").unwrap();
        assert_eq!(f.product, "vert_outbound_rel");
        assert!(f.storm_relative);
    }

    /// The bug this guards: plain/`_rel`/`_fall` variants of the same
    /// product at the same analysis time must never collapse into the same
    /// (product, format, analysis_time) key, or the DB's ON CONFLICT upsert
    /// silently keeps only one of them.
    #[test]
    fn rel_and_fall_variants_stay_distinct_products() {
        let plain = parse_product_filename("240630I1_1201_vert_inbound.nc.gz").unwrap();
        let rel = parse_product_filename("240630I1_1201_vert_inbound_rel.nc.gz").unwrap();
        let fall = parse_product_filename("240630I1_1201_vert_inbound_fall.nc.gz").unwrap();
        assert_ne!(plain.product, rel.product);
        assert_ne!(plain.product, fall.product);
        assert_ne!(rel.product, fall.product);
    }

    #[test]
    fn parses_awips_products() {
        let f = parse_product_filename("AWIPSMaxdb_240630I1_1201z.nc.gz").unwrap();
        assert_eq!(f.product, "awips_maxdb");
        let f = parse_product_filename("AWIPSWindComponents_240630I1_1201z.nc.gz").unwrap();
        assert_eq!(f.product, "awips_wind");
    }

    #[test]
    fn ignores_non_product_files() {
        assert!(parse_product_filename("240630I1_1127_1228_analysis.tar").is_none());
        assert!(parse_product_filename("240630I1_1127_1228_radials.so.gz").is_none());
        assert!(parse_product_filename("20240630123554_20240630I1_120152_jobfile.tar.gz").is_none());
    }

    #[test]
    fn parses_leg_bundle_filenames() {
        let (start, stop) = parse_leg_filename("240630I1_1127_1228_analysis.tar").unwrap();
        assert_eq!(start, "1127");
        assert_eq!(stop, "1228");
        // .gz variant and case-insensitive aircraft letter, seen on both hosts.
        let (start, stop) = parse_leg_filename("180708h1_1012_1158_analysis.tar.gz").unwrap();
        assert_eq!(start, "1012");
        assert_eq!(stop, "1158");
        assert!(parse_leg_filename("240630I1_1127_radials.so.gz").is_none());
        assert!(parse_leg_filename("240630I1_1127_xy.nc.gz").is_none());
    }

    #[test]
    fn mission_id_regex_matches_real_examples() {
        assert!(mission_id_re().is_match("20240630I1"));
        assert!(mission_id_re().is_match("20201008I1a"));
        assert!(!mission_id_re().is_match("20181009H2test"));
        assert!(!mission_id_re().is_match("archive"));
    }

    #[test]
    fn aircraft_letters() {
        assert_eq!(aircraft_from_mission_id("20240630H1").1, Some("N42".into()));
        assert_eq!(aircraft_from_mission_id("20240630I1").1, Some("N43".into()));
        assert_eq!(aircraft_from_mission_id("20240630N1").1, Some("N49".into()));
    }

    /// Trimmed from the real 20251028H1 1420 jobfile (Melissa), which HRD
    /// marked not acceptable for composite.
    const MELISSA_1420_JOBFILE: &str = concat!(
        r#"<?xml version="1.0" encoding="UTF-8"?><flight id="20251028H1" mission="2313A MELISSA" storm="AL132025" mode="0">"#,
        "<centerTime>142007</centerTime><latDeg>17.655</latDeg><latMin>0</latMin><latUnits>0</latUnits>",
        "<lonDeg>76.850</lonDeg><lonMin>0</lonMin><lonUnits>0</lonUnits><lonHemisphere>0</lonHemisphere>",
        "<stmDir>20.00</stmDir><stmMotion>6.00</stmMotion><eventType>3</eventType><acceptable>0</acceptable></flight>",
        "Flight ID: 20251028H1\nCenter lat, lon:     17.655,    -76.850\nAcceptable for composite: 0 (no)\n",
    );

    #[test]
    fn parses_analysis_from_real_jobfile() {
        let a = parse_jobfile_analysis(MELISSA_1420_JOBFILE, "20251028144029_20251028H1_142007_jobfile.tar.gz").unwrap();
        assert_eq!(a.analysis_time, "1420");
        assert_eq!(a.center_time, "142007");
        assert_eq!(a.center_lat, Some(17.655));
        assert_eq!(a.center_lon, Some(-76.850));
        assert_eq!(a.storm_dir_deg, Some(20.0));
        assert_eq!(a.storm_speed_kt, Some(6.0));
        assert_eq!(a.acceptable_for_composite, Some(false));
    }

    #[test]
    fn analysis_falls_back_to_xml_center_and_filename_time() {
        // No summary member and no <centerTime>: center from latDeg/lonDeg
        // (lonHemisphere 0 = west), time from the filename suffix.
        let text = "<flight mission=\"0114A MILTON\"><latDeg>22.540</latDeg><lonDeg>94.740</lonDeg>\
                    <lonHemisphere>0</lonHemisphere><acceptable>1</acceptable></flight>";
        let a = parse_jobfile_analysis(text, "20241006I1_121155_jobfile.tar.gz").unwrap();
        assert_eq!(a.analysis_time, "1211");
        assert_eq!(a.center_lat, Some(22.54));
        assert_eq!(a.center_lon, Some(-94.74));
        assert_eq!(a.acceptable_for_composite, Some(true));
        // Summary-only acceptability.
        let a = parse_jobfile_analysis("Acceptable for composite: 1 (yes)", "x_134908_jobfile.tar.gz").unwrap();
        assert_eq!(a.acceptable_for_composite, Some(true));
        // No time anywhere -> not an analysis we can key.
        assert!(parse_jobfile_analysis("<acceptable>1</acceptable>", "junk.tar.gz").is_none());
    }

    #[test]
    fn jobfile_names_sorts_reruns_after_originals() {
        let hrefs = vec![
            "20251028144029_20251028H1_142007_jobfile.tar.gz".to_string(),
            "251028H1_1349_xy.nc.gz".to_string(),
            "20251028140823_20251028H1_134908_jobfile.tar.gz".to_string(),
        ];
        assert_eq!(
            jobfile_names(&hrefs),
            vec![
                "20251028140823_20251028H1_134908_jobfile.tar.gz".to_string(),
                "20251028144029_20251028H1_142007_jobfile.tar.gz".to_string(),
            ]
        );
    }

    #[test]
    fn upsert_analysis_overwrites_a_rerun() {
        let conn = mem_conn();
        upsert_mission(&conn, "20251028H1", 2025, None, None, Some("Melissa"), None, 1, 0, 100).unwrap();
        let mut a = parse_jobfile_analysis(MELISSA_1420_JOBFILE, "x").unwrap();
        upsert_analysis(&conn, "20251028H1", Level::L1b, &a, "https://example/a", 100).unwrap();
        a.acceptable_for_composite = Some(true);
        upsert_analysis(&conn, "20251028H1", Level::L1b, &a, "https://example/b", 200).unwrap();

        let rows = tdr::get_mission_analyses(&conn, "20251028H1").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].acceptable_for_composite, Some(true));
        assert_eq!(rows[0].source_url, "https://example/b");
    }

    #[test]
    fn indexed_missions_keep_being_rescanned_until_stable() {
        let conn = mem_conn();
        let now = 1_000_000_000;
        // Never indexed at this level -> crawl.
        assert!(!should_skip_crawl(&conn, "20251028H1", Level::L1b, false, now));

        upsert_mission(&conn, "20251028H1", 2025, None, None, None, None, 1, 0, now).unwrap();
        // Indexed before crawl_state existed (no row) -> re-list once.
        assert!(!should_skip_crawl(&conn, "20251028H1", Level::L1b, false, now));

        // Indexed and changed recently (a partial, in-progress mission) -> re-list.
        record_crawl(&conn, "20251028H1", Level::L1b, now, true).unwrap();
        assert!(!should_skip_crawl(&conn, "20251028H1", Level::L1b, false, now + 3600));

        // Quiet re-lists don't reset the clock...
        record_crawl(&conn, "20251028H1", Level::L1b, now + 86_400, false).unwrap();
        // ...so once it's been quiet past the window, it's skipped.
        let later = now + STABLE_AFTER_SECS + 1;
        assert!(should_skip_crawl(&conn, "20251028H1", Level::L1b, false, later));
        // `force` always crawls; the other level is tracked independently.
        assert!(!should_skip_crawl(&conn, "20251028H1", Level::L1b, true, later));
        assert!(!should_skip_crawl(&conn, "20251028H1", Level::L2, false, later));

        // A new file showing up restarts the window.
        record_crawl(&conn, "20251028H1", Level::L1b, later, true).unwrap();
        assert!(!should_skip_crawl(&conn, "20251028H1", Level::L1b, false, later + 1));
    }
}
