//! Tail Doppler Radar (TDR) archive — READ path.
//!
//! Schema note: TDR `mission_id`s (`YYYYMMDDAI`) use the *exact same* scheme as
//! the recon MET archive (see recon_met.rs) — same year/date/aircraft-letter/
//! flight-sequence convention, confirmed against a live crawl of both hosts.
//! That means storm identity for TDR is resolved *live at read time* by LEFT
//! JOINing `recon.missions` on the shared `mission_id`, rather than storing a
//! denormalized name and re-crawling to reconcile it — see `get_connection`
//! (which ATTACHes the recon DB) and `tdr_ingest.rs`.
//!
//! Two source levels, same file-naming convention, different hosts and QC
//! lineage (see the AOML TDR README this was built against):
//! - **Level 1b** (real-time, in-season): `seb.omao.noaa.gov/pub/flight/radar/{mission_id}/`
//!   — flat mission directories, no storm name in the path.
//! - **Level 2** (post-season, QC'd): `www.aoml.noaa.gov/ftp/pub/hrd/data/radar/level2/{year}/{storm_slug}/{mission_id}/`
//!   — storm name is part of the path itself.
//!
//! This module only indexes file *metadata* (source URLs) — actual netCDF
//! download/decompress happens lazily on first request, same pattern as
//! `cache/goes_nc/` (see `goes.rs`), so ingest never has to bulk-download the
//! archive.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Row};
use serde::Serialize;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS missions (
    mission_id  TEXT PRIMARY KEY,
    year        INTEGER NOT NULL,
    aircraft    TEXT,
    tail_num    TEXT,
    -- Ingest-time storm-name *fallback* only — the jobfile name (Level 1b) or
    -- path slug (Level 2) captured at crawl time. Storm identity served to
    -- callers is resolved live from the attached recon index (see
    -- `get_connection`); this is what a read falls back to when recon has no
    -- row for the mission yet. No `storm_source`/`pending` reconciliation
    -- machine exists anymore — the join *is* the reconciliation.
    storm_label TEXT,
    storm_id    TEXT,
    has_level1b INTEGER NOT NULL DEFAULT 0,
    has_level2  INTEGER NOT NULL DEFAULT 0,
    fetched_at  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_tdr_missions_year ON missions(year);

CREATE TABLE IF NOT EXISTS files (
    id                 INTEGER PRIMARY KEY,
    mission_id         TEXT NOT NULL REFERENCES missions(mission_id) ON DELETE CASCADE,
    level              TEXT NOT NULL,
    product            TEXT NOT NULL,
    format             TEXT NOT NULL,
    analysis_time      TEXT NOT NULL,
    storm_relative     INTEGER NOT NULL DEFAULT 0,
    fall_speed_removed INTEGER NOT NULL DEFAULT 0,
    source_url         TEXT NOT NULL,
    fetched_at         INTEGER NOT NULL,
    UNIQUE(mission_id, level, product, format, analysis_time)
);
CREATE INDEX IF NOT EXISTS idx_tdr_files_mission ON files(mission_id);

-- One row per radar leg (radar started, flown, stopped) — start_time/stop_time
-- are the real HHMM bounds lifted straight from that leg's
-- `{prefix}_{start}_{stop}_analysis.tar` bundle filename (see tdr_ingest.rs),
-- not derived/guessed from the analysis_times of the xy/vert files it produced.
CREATE TABLE IF NOT EXISTS legs (
    id          INTEGER PRIMARY KEY,
    mission_id  TEXT NOT NULL REFERENCES missions(mission_id) ON DELETE CASCADE,
    level       TEXT NOT NULL,
    start_time  TEXT NOT NULL,
    stop_time   TEXT NOT NULL,
    source_url  TEXT NOT NULL,
    fetched_at  INTEGER NOT NULL,
    UNIQUE(mission_id, level, start_time, stop_time)
);
CREATE INDEX IF NOT EXISTS idx_tdr_legs_mission ON legs(mission_id);
";

#[derive(Debug, Clone, Serialize)]
pub struct Mission {
    pub mission_id: String,
    pub year: i64,
    pub aircraft: Option<String>,
    pub tail_num: Option<String>,
    /// Resolved storm name — `COALESCE(recon.storm_name, storm_label, 'Unknown')`
    /// produced by the read queries, never a stored column. See `MISSION_SELECT`.
    pub storm_name: String,
    /// Resolved storm id — `COALESCE(recon.storm_id, storm_id)`.
    pub storm_id: Option<String>,
    pub has_level1b: bool,
    pub has_level2: bool,
}

impl Mission {
    fn from_row(row: &Row) -> rusqlite::Result<Self> {
        Ok(Self {
            mission_id: row.get("mission_id")?,
            year: row.get("year")?,
            aircraft: row.get("aircraft")?,
            tail_num: row.get("tail_num")?,
            storm_name: row.get("storm_name")?,
            storm_id: row.get("storm_id")?,
            has_level1b: row.get::<_, i64>("has_level1b")? != 0,
            has_level2: row.get::<_, i64>("has_level2")? != 0,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct StormSummary {
    pub storm_name: String,
    pub storm_id: Option<String>,
    pub mission_count: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileRecord {
    pub level: String,
    pub product: String,
    pub format: String,
    pub analysis_time: String,
    pub storm_relative: bool,
    pub fall_speed_removed: bool,
    pub source_url: String,
}

impl FileRecord {
    fn from_row(row: &Row) -> rusqlite::Result<Self> {
        Ok(Self {
            level: row.get("level")?,
            product: row.get("product")?,
            format: row.get("format")?,
            analysis_time: row.get("analysis_time")?,
            storm_relative: row.get::<_, i64>("storm_relative")? != 0,
            fall_speed_removed: row.get::<_, i64>("fall_speed_removed")? != 0,
            source_url: row.get("source_url")?,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LegRecord {
    pub level: String,
    pub start_time: String,
    pub stop_time: String,
    pub source_url: String,
}

impl LegRecord {
    fn from_row(row: &Row) -> rusqlite::Result<Self> {
        Ok(Self {
            level: row.get("level")?,
            start_time: row.get("start_time")?,
            stop_time: row.get("stop_time")?,
            source_url: row.get("source_url")?,
        })
    }
}

/// The WRITE path (startup init + ingest). Opens `db_path`, turns foreign keys
/// on, applies the current `SCHEMA`, and runs the one-time table rebuild for
/// pre-change DBs (`storm_name`/`storm_source`/`pending` → `storm_label`). This
/// is the only place the schema is created/migrated — read connections
/// deliberately never touch DDL, so `init_db` must run at startup before any
/// `get_connection` opens.
pub fn init_db(db_path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(db_path)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.execute_batch(SCHEMA)?;
    migrate_rebuild(&conn)?;
    Ok(conn)
}

/// The READ path used by every handler. Opens `tdr_db` read-only and ATTACHes
/// the recon index as `recon` so storm identity can be resolved live in a LEFT
/// JOIN (see `MISSION_SELECT`) — the whole point of dropping the stored
/// association. It runs no DDL/migration/UPDATE and is set `query_only` so it
/// can never write either database.
///
/// Dependency: this assumes `recon.missions` exists (it's created and populated
/// by recon ingest and is always present in `data/recon_met.sqlite`). SQLite
/// creates/opens the file on ATTACH, and the LEFT JOIN degrades gracefully — a
/// missing recon row for a mission just yields NULLs that COALESCE down to
/// `storm_label`/'Unknown' — but the `recon.missions` *table* must exist for
/// the join to parse, which it does in this repo.
pub fn get_connection(tdr_db: &Path, recon_db: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(tdr_db)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    // Path as a bound string (lossy is fine — these are ASCII repo paths).
    let recon_str = recon_db.to_string_lossy();
    conn.execute("ATTACH DATABASE ?1 AS recon", [recon_str.as_ref()])?;
    // Belt-and-braces: this connection must never mutate either DB.
    conn.pragma_update(None, "query_only", "ON")?;
    Ok(conn)
}

/// One-time rebuild of the `missions` table for DBs created under the old
/// schema (denormalized `storm_name` + the `storm_source`/`pending`
/// reconciliation columns). The index is fully re-crawlable, so rather than a
/// column-by-column dance we detect any old marker column via
/// `PRAGMA table_info` and, if present, do SQLite's recommended table rebuild:
/// copy into a fresh table on the new schema (folding the old `storm_name` into
/// `storm_label`, dropping the sentinel 'Unknown'), drop the old table, rename.
/// Foreign keys are turned off around the rebuild per SQLite's guidance (the
/// `files`/`legs` FKs reference `missions(mission_id)`), then back on. A fresh
/// DB created straight from `SCHEMA` has no marker column and is left untouched.
fn migrate_rebuild(conn: &Connection) -> rusqlite::Result<()> {
    let needs_rebuild = {
        let mut stmt = conn.prepare("PRAGMA table_info(missions)")?;
        let cols: Vec<String> = stmt.query_map([], |r| r.get::<_, String>(1))?.collect::<Result<_, _>>()?;
        cols.iter().any(|c| c == "storm_name" || c == "storm_source" || c == "pending")
    };
    if !needs_rebuild {
        return Ok(());
    }

    // FK enforcement can't change inside a transaction, so toggle it outside.
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    let res = conn.execute_batch(
        "BEGIN;
         CREATE TABLE missions_new (
             mission_id  TEXT PRIMARY KEY,
             year        INTEGER NOT NULL,
             aircraft    TEXT,
             tail_num    TEXT,
             storm_label TEXT,
             storm_id    TEXT,
             has_level1b INTEGER NOT NULL DEFAULT 0,
             has_level2  INTEGER NOT NULL DEFAULT 0,
             fetched_at  INTEGER NOT NULL
         );
         INSERT INTO missions_new
             (mission_id, year, aircraft, tail_num, storm_label, storm_id, has_level1b, has_level2, fetched_at)
         SELECT mission_id, year, aircraft, tail_num, NULLIF(storm_name, 'Unknown'), storm_id,
                has_level1b, has_level2, fetched_at
         FROM missions;
         DROP TABLE missions;
         ALTER TABLE missions_new RENAME TO missions;
         CREATE INDEX IF NOT EXISTS idx_tdr_missions_year ON missions(year);
         COMMIT;",
    );
    // Restore FK enforcement regardless of outcome; roll back a failed rebuild.
    if res.is_err() {
        let _ = conn.execute_batch("ROLLBACK");
    }
    conn.pragma_update(None, "foreign_keys", "ON")?;
    res
}

/// The resolved-identity SELECT shared by every mission read. Storm identity is
/// computed live at read time by LEFT JOINing the ATTACHed recon index on the
/// shared `mission_id`, falling back to the ingest-time `storm_label`, then
/// 'Unknown'. Read connections carry `recon` (see `get_connection`); the write
/// connection never runs these.
const MISSION_SELECT: &str = "\
    SELECT t.mission_id, t.year, t.aircraft, t.tail_num, \
           COALESCE(recon.storm_name, t.storm_label, 'Unknown') AS storm_name, \
           COALESCE(recon.storm_id, t.storm_id) AS storm_id, \
           t.has_level1b, t.has_level2 \
    FROM missions t \
    LEFT JOIN recon.missions recon ON recon.mission_id = t.mission_id";

pub fn list_years(conn: &Connection) -> rusqlite::Result<Vec<i64>> {
    let mut stmt = conn.prepare("SELECT DISTINCT year FROM missions ORDER BY year")?;
    let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
    rows.collect()
}

pub fn list_storms_for_year(conn: &Connection, year: i64) -> rusqlite::Result<Vec<StormSummary>> {
    // Group by the *resolved* name (recon → storm_label → 'Unknown'), so two
    // missions that resolve to the same storm via recon collapse into one row
    // even if their stored labels differ.
    let mut stmt = conn.prepare(
        "SELECT COALESCE(recon.storm_name, t.storm_label, 'Unknown') AS storm_name, \
                COUNT(*) AS mission_count, \
                MAX(COALESCE(recon.storm_id, t.storm_id)) AS storm_id \
         FROM missions t \
         LEFT JOIN recon.missions recon ON recon.mission_id = t.mission_id \
         WHERE t.year = ?1 GROUP BY storm_name ORDER BY storm_name",
    )?;
    let rows = stmt.query_map([year], |r| {
        Ok(StormSummary {
            storm_name: r.get("storm_name")?,
            mission_count: r.get("mission_count")?,
            storm_id: r.get("storm_id")?,
        })
    })?;
    rows.collect()
}

pub fn list_missions_for_storm(
    conn: &Connection,
    year: i64,
    storm_name: &str,
) -> rusqlite::Result<Vec<Mission>> {
    // Match against the resolved name, not a stored column — mirrors how
    // `list_storms_for_year` groups, so the storm links there round-trip here.
    let sql = format!(
        "{MISSION_SELECT} \
         WHERE t.year = ?1 \
           AND COALESCE(recon.storm_name, t.storm_label, 'Unknown') = ?2 COLLATE NOCASE \
         ORDER BY t.mission_id"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params![year, storm_name], Mission::from_row)?;
    rows.collect()
}

pub fn get_mission(conn: &Connection, mission_id: &str) -> rusqlite::Result<Option<Mission>> {
    conn.query_row(
        &format!("{MISSION_SELECT} WHERE t.mission_id = ?1"),
        [mission_id],
        Mission::from_row,
    )
    .optional()
}

pub fn get_mission_files(conn: &Connection, mission_id: &str) -> rusqlite::Result<Vec<FileRecord>> {
    let mut stmt = conn.prepare(
        "SELECT * FROM files WHERE mission_id = ?1 ORDER BY level, analysis_time, product",
    )?;
    let rows = stmt.query_map([mission_id], FileRecord::from_row)?;
    rows.collect()
}

/// A mission's radar legs, chronological — see the `legs` table doc comment
/// in `SCHEMA` for where start_time/stop_time actually come from.
pub fn get_mission_legs(conn: &Connection, mission_id: &str) -> rusqlite::Result<Vec<LegRecord>> {
    let mut stmt = conn.prepare(
        "SELECT * FROM legs WHERE mission_id = ?1 ORDER BY start_time",
    )?;
    let rows = stmt.query_map([mission_id], LegRecord::from_row)?;
    rows.collect()
}

/// Every analysis_time's netCDF file for one (mission, level, product) —
/// used by `GET /v1/tdr/composite?mode=time` to mosaic a CAPPI level across
/// a mission's whole timeline. Sorted by `analysis_time` so callers get a
/// deterministic, chronological composite order.
pub fn find_files_for_product(
    conn: &Connection,
    mission_id: &str,
    level: &str,
    product: &str,
) -> rusqlite::Result<Vec<FileRecord>> {
    let mut stmt = conn.prepare(
        "SELECT * FROM files WHERE mission_id = ?1 AND level = ?2 AND product = ?3 AND format = 'nc' \
         ORDER BY analysis_time",
    )?;
    let rows = stmt.query_map(rusqlite::params![mission_id, level, product], FileRecord::from_row)?;
    rows.collect()
}

/// One exact (mission, level, product, analysis_time, format) file record —
/// used by `GET /v1/tdr/sweep` to resolve the source URL to fetch/cache.
pub fn find_file(
    conn: &Connection,
    mission_id: &str,
    level: &str,
    product: &str,
    analysis_time: &str,
    format: &str,
) -> rusqlite::Result<Option<FileRecord>> {
    conn.query_row(
        "SELECT * FROM files WHERE mission_id = ?1 AND level = ?2 AND product = ?3 \
         AND analysis_time = ?4 AND format = ?5",
        rusqlite::params![mission_id, level, product, analysis_time, format],
        FileRecord::from_row,
    )
    .optional()
}
