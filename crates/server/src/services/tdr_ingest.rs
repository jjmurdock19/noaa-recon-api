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
//! Only the gridded analysis products are indexed (`xy`/`xy_rel`, the
//! `vert_inbound`/`vert_outbound` profiles, and the two AWIPS derivatives) —
//! the ancillary `analysis.tar`/`radials.so.gz`/`jobfile.tar.gz` bundles
//! aren't netCDF and aren't what a future slice/passthrough endpoint would
//! ever read, so there's no reason to index them.
//!
//! Storm-name resolution piggybacks on the recon MET archive where
//! possible: TDR `mission_id`s (`YYYYMMDDAI`) use the exact same scheme as
//! recon MET missions (confirmed against a live crawl of both hosts), so a
//! mission already reconciled to a storm there doesn't need re-deriving.

use std::path::Path;
use std::sync::OnceLock;

use chrono::{Datelike, Utc};
use regex::Regex;
use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::services::recon_met;
use crate::services::tdr;

const LEVEL1B_BASE: &str = "https://seb.omao.noaa.gov/pub/flight/radar";
const LEVEL2_BASE: &str = "https://www.aoml.noaa.gov/ftp/pub/hrd/data/radar/level2";
const HTTP_TIMEOUT_SECS: u64 = 30;
const UNKNOWN_STORM: &str = "Unknown";
const TRAINING_BUCKET_NAME: &str = "Training / Research";

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

fn mission_id_re() -> &'static Regex {
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

fn mission_year(mission_id: &str) -> Option<i64> {
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

/// Best available storm name/id for a mission: a recon MET reconciliation
/// (already haversine-matched against the storms DB) beats a Level 2
/// directory name, which beats whatever this DB already had on file, which
/// beats "Unknown".
fn resolve_storm(
    recon_conn: &Connection,
    tdr_conn: &Connection,
    mission_id: &str,
    level2_storm_slug: Option<&str>,
) -> rusqlite::Result<(String, Option<String>)> {
    let recon_hit: Option<(String, Option<String>)> = recon_conn
        .query_row(
            "SELECT storm_name, storm_id FROM missions WHERE mission_id = ?1",
            [mission_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
        )
        .optional()?;
    if let Some((name, id)) = recon_hit {
        if !name.eq_ignore_ascii_case(TRAINING_BUCKET_NAME) {
            return Ok((name, id));
        }
    }
    if let Some(slug) = level2_storm_slug {
        return Ok((title_case(slug), None));
    }
    let existing: Option<String> = tdr_conn
        .query_row("SELECT storm_name FROM missions WHERE mission_id = ?1", [mission_id], |r| r.get(0))
        .optional()?;
    Ok((existing.unwrap_or_else(|| UNKNOWN_STORM.to_string()), None))
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ── Per-mission harvest ──────────────────────────────────────────────────────

/// Crawls one mission directory's file listing and upserts its parsed
/// products. Returns whether anything new was indexed. Skips the crawl
/// entirely (no HTTP request against the mission dir) if this level is
/// already on record for the mission, unless `force`.
#[allow(clippy::too_many_arguments)]
async fn harvest_mission_dir(
    http: &reqwest::Client,
    conn: &Connection,
    recon_conn: &Connection,
    mission_id: &str,
    year: i64,
    level: Level,
    mission_url: &str,
    level2_storm_slug: Option<&str>,
    force: bool,
) -> anyhow::Result<bool> {
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
    if already != 0 && !force {
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

    let (storm_name, storm_id) = resolve_storm(recon_conn, conn, mission_id, level2_storm_slug)?;
    let (aircraft, tail_num) = aircraft_from_mission_id(mission_id);
    let fetched_at = now_unix();

    conn.execute_batch("BEGIN")?;
    let res = (|| -> rusqlite::Result<()> {
        let (level1b_flag, level2_flag) = match level {
            Level::L1b => (1, 0),
            Level::L2 => (0, 1),
        };
        conn.execute(
            "INSERT INTO missions \
             (mission_id, year, aircraft, tail_num, storm_name, storm_id, has_level1b, has_level2, fetched_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9) \
             ON CONFLICT(mission_id) DO UPDATE SET \
               aircraft=excluded.aircraft, tail_num=excluded.tail_num, \
               storm_name=CASE WHEN missions.storm_name = ?10 THEN excluded.storm_name ELSE missions.storm_name END, \
               storm_id=COALESCE(missions.storm_id, excluded.storm_id), \
               has_level1b=MAX(missions.has_level1b, excluded.has_level1b), \
               has_level2=MAX(missions.has_level2, excluded.has_level2), \
               fetched_at=excluded.fetched_at",
            rusqlite::params![
                mission_id, year, aircraft, tail_num, storm_name, storm_id,
                level1b_flag, level2_flag, fetched_at, UNKNOWN_STORM,
            ],
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
        Ok(())
    })();
    match res {
        Ok(()) => {
            conn.execute_batch("COMMIT")?;
            tracing::info!("{mission_id} ({}): indexed {} product file(s)", level.as_str(), files.len());
            Ok(true)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e.into())
        }
    }
}

// ── Orchestration ────────────────────────────────────────────────────────────

/// Full TDR ingest (`run_ingest`). `years` defaults to [current-1, current],
/// same as the recon MET archive. Crawls Level 1b (flat mission dirs, all
/// years live under one listing so the `years` filter is applied after the
/// fact) and Level 2 (year -> storm -> mission) for each requested year.
pub async fn run_ingest(
    tdr_db: &Path,
    recon_met_db: &Path,
    years: Option<Vec<i64>>,
    force: bool,
) -> anyhow::Result<Value> {
    let years = years.unwrap_or_else(|| {
        let y = Utc::now().year() as i64;
        vec![y - 1, y]
    });

    let conn = tdr::get_connection(tdr_db)?;
    let recon_conn = recon_met::get_connection(recon_met_db)?;
    let http = client()?;

    let (mut ingested_1b, mut ingested_2, mut skipped, mut errors) = (0i64, 0i64, 0i64, 0i64);

    for mission_id in get_level1b_mission_list(&http).await {
        let Some(year) = mission_year(&mission_id) else { continue };
        if !years.contains(&year) {
            continue;
        }
        let mission_url = format!("{LEVEL1B_BASE}/{mission_id}/");
        match harvest_mission_dir(&http, &conn, &recon_conn, &mission_id, year, Level::L1b, &mission_url, None, force)
            .await
        {
            Ok(true) => ingested_1b += 1,
            Ok(false) => skipped += 1,
            Err(e) => {
                tracing::error!("{mission_id} (Level 1b): {e}");
                errors += 1;
            }
        }
    }

    for year in &years {
        for slug in get_level2_storm_slugs(&http, *year).await {
            for mission_id in get_level2_mission_list(&http, *year, &slug).await {
                let mission_url = format!("{LEVEL2_BASE}/{year}/{slug}/{mission_id}/");
                match harvest_mission_dir(
                    &http,
                    &conn,
                    &recon_conn,
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
        }
    }

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
}
