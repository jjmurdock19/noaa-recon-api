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
//! Storm-name resolution piggybacks on the recon MET archive where
//! possible: TDR `mission_id`s (`YYYYMMDDAI`) use the exact same scheme as
//! recon MET missions (confirmed against a live crawl of both hosts), so a
//! mission already reconciled to a storm there doesn't need re-deriving.

use std::io::Read;
use std::path::Path;
use std::sync::OnceLock;

use chrono::{Datelike, Utc};
use flate2::read::GzDecoder;
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

/// Where a mission's storm name came from, lowest→highest confidence. Ranking
/// is what lets a re-crawl *upgrade* a name (jobfile → recon) without ever
/// downgrading a confirmed one. `Level2`/`Recon` are "confirmed" (storm pinned
/// by a QC'd path or a track-match against the storms DB); `Unknown`/`Jobfile`
/// are provisional and keep the mission `pending` so it self-heals later.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum StormSource {
    Unknown = 0,
    Jobfile = 1,
    Level2 = 2,
    Recon = 3,
}

impl StormSource {
    fn as_str(self) -> &'static str {
        match self {
            StormSource::Unknown => "unknown",
            StormSource::Jobfile => "jobfile",
            StormSource::Level2 => "level2",
            StormSource::Recon => "recon",
        }
    }

    fn from_str(s: &str) -> Self {
        match s {
            "jobfile" => StormSource::Jobfile,
            "level2" => StormSource::Level2,
            "recon" => StormSource::Recon,
            _ => StormSource::Unknown,
        }
    }

    /// Confirmed = storm attribution is trustworthy and the mission is no
    /// longer `pending`. Only a Level 2 path or a recon-MET track-match count.
    fn confirmed(self) -> bool {
        self >= StormSource::Level2
    }
}

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

/// The storm name + ATCF id lifted from a Level 1b mission dir's own
/// `*_jobfile.tar.gz`. The gzip wraps a tiny (~1.6 KB) tar whose `jobfile.xml`
/// member is a single line like
/// `<flight id="20251030H1" mission="3113A MELISSA" storm="AL132025" …>`.
/// Because tar stores member *contents* uncompressed, the gunzipped bytes carry
/// those attributes verbatim — so we gunzip and regex them straight out rather
/// than pull in a tar reader for one 1.6 KB blob. This is the ONLY in-directory
/// source of the storm name for a Level 1b mission (no plaintext index exists),
/// which is exactly what lets a radar-only mission that landed before its recon
/// MET data get a real (if still provisional) name instead of "Unknown".
async fn fetch_jobfile_storm(
    http: &reqwest::Client,
    mission_url: &str,
    hrefs: &[String],
) -> Option<(String, Option<String>)> {
    let job_href = hrefs.iter().find(|h| h.to_lowercase().ends_with("_jobfile.tar.gz"))?;
    let name = job_href.rsplit('/').next().unwrap_or(job_href);
    let gz = fetch_bytes(http, &format!("{mission_url}{name}")).await?;
    let mut text = String::new();
    // The tar's binary headers aren't valid UTF-8, so read as bytes then
    // lossy-decode — the `<flight …>` line we want is plain ASCII regardless.
    let mut buf = Vec::new();
    GzDecoder::new(&gz[..]).read_to_end(&mut buf).ok()?;
    text.push_str(&String::from_utf8_lossy(&buf));
    parse_jobfile_storm(&text)
}

/// Pulls `(storm_name, atcf_id)` from a gunzipped jobfile tar's text (the
/// `<flight mission="…" storm="…">` line). Split out for unit testing.
fn parse_jobfile_storm(text: &str) -> Option<(String, Option<String>)> {
    static MISSION_RE: OnceLock<Regex> = OnceLock::new();
    static STORM_RE: OnceLock<Regex> = OnceLock::new();
    let mission_re = MISSION_RE.get_or_init(|| Regex::new(r#"mission="([^"]*)""#).unwrap());
    let storm_re = STORM_RE.get_or_init(|| Regex::new(r#"storm="([A-Za-z]{2}\d{6})""#).unwrap());

    let mission_val = mission_re.captures(text)?.get(1)?.as_str().trim().to_string();
    // "3113A MELISSA" → the name is everything after the leading flight-number
    // token; a training/ferry jobfile is just the number with no name → skip.
    let mut parts = mission_val.split_whitespace();
    let _flight_num = parts.next()?;
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

/// Best available `(storm_name, storm_id, source)` for a mission, picked by
/// confidence rank so a re-crawl can *upgrade* a provisional name but never
/// downgrades a confirmed one:
///   recon MET track-match  >  Level 2 path  >  jobfile guess  >  what this DB
///   already resolved  >  "Unknown".
/// `jobfile` is the `(name, atcf)` already parsed from the mission dir (only
/// Level 1b dirs carry one), passed in because the fetch is async and this is
/// sync. The returned `source` drives the `pending` flag: anything below
/// Level 2 stays pending so it keeps re-resolving until recon data lands.
fn resolve_storm(
    recon_conn: &Connection,
    tdr_conn: &Connection,
    mission_id: &str,
    level2_storm_slug: Option<&str>,
    jobfile: Option<(String, Option<String>)>,
) -> rusqlite::Result<(String, Option<String>, StormSource)> {
    let mut candidates: Vec<(StormSource, String, Option<String>)> = Vec::new();

    let recon_hit: Option<(String, Option<String>)> = recon_conn
        .query_row(
            "SELECT storm_name, storm_id FROM missions WHERE mission_id = ?1",
            [mission_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
        )
        .optional()?;
    if let Some((name, id)) = recon_hit {
        if !name.eq_ignore_ascii_case(TRAINING_BUCKET_NAME) {
            candidates.push((StormSource::Recon, name, id));
        }
    }
    if let Some(slug) = level2_storm_slug {
        candidates.push((StormSource::Level2, title_case(slug), None));
    }
    if let Some((name, id)) = jobfile {
        candidates.push((StormSource::Jobfile, name, id));
    }
    // Whatever this DB already had — carried in at its stored rank so a prior
    // recon/level2 confirmation is never lost to a later jobfile-only re-crawl.
    let existing: Option<(String, Option<String>, String)> = tdr_conn
        .query_row(
            "SELECT storm_name, storm_id, storm_source FROM missions WHERE mission_id = ?1",
            [mission_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    if let Some((name, id, src)) = existing {
        if !name.eq_ignore_ascii_case(UNKNOWN_STORM) {
            candidates.push((StormSource::from_str(&src), name, id));
        }
    }

    // Highest rank wins; on a tie the later push (existing) wins, keeping its id.
    Ok(candidates
        .into_iter()
        .max_by_key(|(s, _, _)| *s)
        .map(|(s, n, i)| (n, i, s))
        .unwrap_or_else(|| (UNKNOWN_STORM.to_string(), None, StormSource::Unknown)))
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
    let (already, pending): (i64, i64) = conn
        .query_row(
            match level {
                Level::L1b => "SELECT has_level1b, pending FROM missions WHERE mission_id = ?1",
                Level::L2 => "SELECT has_level2, pending FROM missions WHERE mission_id = ?1",
            },
            [mission_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap_or((0, 0));
    // Skip the crawl only when this level is already indexed AND the storm is
    // already confirmed. A `pending` mission (radar in, storm not yet pinned)
    // is deliberately re-crawled every run so it self-heals the moment its
    // recon MET data uploads — that's the whole fix for the "landed as Unknown"
    // case. `force` overrides regardless.
    if already != 0 && !force && pending == 0 {
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

    // Only Level 1b dirs carry a jobfile; a Level 2 mission's storm is already
    // pinned by its path, so don't spend the fetch there.
    let jobfile = if level == Level::L1b {
        fetch_jobfile_storm(http, mission_url, &hrefs).await
    } else {
        None
    };
    let (storm_name, storm_id, source) =
        resolve_storm(recon_conn, conn, mission_id, level2_storm_slug, jobfile)?;
    let pending_flag: i64 = if source.confirmed() { 0 } else { 1 };
    let (aircraft, tail_num) = aircraft_from_mission_id(mission_id);
    let fetched_at = now_unix();

    conn.execute_batch("BEGIN")?;
    let res = (|| -> rusqlite::Result<()> {
        let (level1b_flag, level2_flag) = match level {
            Level::L1b => (1, 0),
            Level::L2 => (0, 1),
        };
        // `resolve_storm` already folded the existing row in by confidence
        // rank, so `excluded.*` is the best name/source — write it straight
        // (no CASE): an upgrade lands, and a same-or-lower re-crawl re-writes
        // the identical value. storm_id keeps a previously-found ATCF if this
        // pass didn't supply one.
        conn.execute(
            "INSERT INTO missions \
             (mission_id, year, aircraft, tail_num, storm_name, storm_id, has_level1b, has_level2, \
              storm_source, pending, fetched_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11) \
             ON CONFLICT(mission_id) DO UPDATE SET \
               aircraft=excluded.aircraft, tail_num=excluded.tail_num, \
               storm_name=excluded.storm_name, \
               storm_id=COALESCE(excluded.storm_id, missions.storm_id), \
               storm_source=excluded.storm_source, \
               pending=excluded.pending, \
               has_level1b=MAX(missions.has_level1b, excluded.has_level1b), \
               has_level2=MAX(missions.has_level2, excluded.has_level2), \
               fetched_at=excluded.fetched_at",
            rusqlite::params![
                mission_id, year, aircraft, tail_num, storm_name, storm_id,
                level1b_flag, level2_flag, source.as_str(), pending_flag, fetched_at,
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

        let mut leg_stmt = conn.prepare(
            "INSERT INTO legs (mission_id, level, start_time, stop_time, source_url, fetched_at) \
             VALUES (?1,?2,?3,?4,?5,?6) \
             ON CONFLICT(mission_id, level, start_time, stop_time) DO UPDATE SET \
               source_url=excluded.source_url, fetched_at=excluded.fetched_at",
        )?;
        for (start, stop, url) in &legs {
            leg_stmt.execute(rusqlite::params![mission_id, level.as_str(), start, stop, url, fetched_at])?;
        }
        Ok(())
    })();
    match res {
        Ok(()) => {
            conn.execute_batch("COMMIT")?;
            tracing::info!(
                "{mission_id} ({}): indexed {} product file(s), {} leg(s)",
                level.as_str(),
                files.len(),
                legs.len()
            );
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

/// Re-run storm resolution for missions whose attribution isn't confirmed
/// (`pending`/`Unknown`), or an explicit `mission_ids` list, by force-crawling
/// each one's Level 1b dir + jobfile and re-checking the recon MET DB. This is
/// what the admin console's "fix pending TDR" and per-flight buttons call:
/// once a flight's recon MET data finally uploads (and recon ingest has run),
/// this promotes the mission out of the pending/Unknown bucket to its real
/// storm. Nothing here re-crawls the recon archive itself — run recon ingest
/// (or the per-mission recon re-ingest) first if the recon data is what's
/// missing.
pub async fn reresolve_missions(
    tdr_db: &Path,
    recon_met_db: &Path,
    mission_ids: Option<Vec<String>>,
) -> anyhow::Result<Value> {
    let conn = tdr::get_connection(tdr_db)?;
    let recon_conn = recon_met::get_connection(recon_met_db)?;
    let http = client()?;

    let targets: Vec<(String, i64)> = match mission_ids {
        Some(ids) => ids.into_iter().filter_map(|id| mission_year(&id).map(|y| (id, y))).collect(),
        None => {
            let mut stmt = conn
                .prepare("SELECT mission_id, year FROM missions WHERE pending = 1 OR storm_name = 'Unknown'")?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
            rows.filter_map(|r| r.ok()).collect()
        }
    };

    let (mut reresolved, mut confirmed, mut errors) = (0i64, 0i64, 0i64);
    let mut results = Vec::new();
    for (mission_id, year) in &targets {
        let mission_url = format!("{LEVEL1B_BASE}/{mission_id}/");
        match harvest_mission_dir(
            &http, &conn, &recon_conn, mission_id, *year, Level::L1b, &mission_url, None, true,
        )
        .await
        {
            Ok(_) => {
                reresolved += 1;
                if let Ok(Some(m)) = tdr::get_mission(&conn, mission_id) {
                    if !m.pending {
                        confirmed += 1;
                    }
                    results.push(json!({
                        "mission_id": mission_id,
                        "storm_name": m.storm_name,
                        "storm_source": m.storm_source,
                        "pending": m.pending,
                    }));
                }
            }
            Err(e) => {
                tracing::error!("{mission_id}: re-resolve failed: {e}");
                errors += 1;
            }
        }
    }

    Ok(json!({
        "targeted": targets.len(),
        "reresolved": reresolved,
        "now_confirmed": confirmed,
        "errors": errors,
        "missions": results,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_storm_from_jobfile_xml() {
        let text = r#"<flight id="20251030H1" mission="3113A MELISSA" storm="AL132025" mode="0"><start3D>174741</start3D></flight>"#;
        let (name, atcf) = parse_jobfile_storm(text).unwrap();
        assert_eq!(name, "Melissa");
        assert_eq!(atcf.as_deref(), Some("AL132025"));
    }

    #[test]
    fn jobfile_without_storm_name_is_none() {
        // ferry/training jobfile: mission is just the flight number, no name.
        assert!(parse_jobfile_storm(r#"<flight id="20250101H1" mission="0101A" storm="">"#).is_none());
        assert!(parse_jobfile_storm(r#"<flight mission="0101A FERRY">"#).is_none());
        assert!(parse_jobfile_storm("no flight element here").is_none());
    }

    #[test]
    fn storm_source_ranks_recon_over_jobfile() {
        assert!(StormSource::Recon > StormSource::Level2);
        assert!(StormSource::Level2 > StormSource::Jobfile);
        assert!(StormSource::Jobfile > StormSource::Unknown);
        assert!(StormSource::Level2.confirmed());
        assert!(!StormSource::Jobfile.confirmed());
        assert!(!StormSource::Unknown.confirmed());
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
}
