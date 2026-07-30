//! Tail Doppler Radar (TDR) archive — READ + WRITE path.
//!
//! Schema note: TDR `mission_id`s (`YYYYMMDDAI`) use the *exact same* scheme as
//! the recon MET archive (see recon_met.rs) — same year/date/aircraft-letter/
//! flight-sequence convention, confirmed against a live crawl of both hosts.
//! Storm identity, though, is entirely TDR's own: `storm_name`/`storm_id` are
//! stored columns captured at ingest time from a same-host source (the Level
//! 1b mission's own `*_jobfile.tar.gz`, or the Level 2 storm-slug path — see
//! `tdr_ingest.rs::fetch_jobfile_storm`), not resolved via a join against the
//! recon MET index. That used to be joined live at read time, but the recon
//! index's own storm-name reconciliation (`recon_ingest.rs`) could — and did —
//! silently overwrite a correct TDR-sourced name with "Training / Research"
//! whenever it couldn't track-match the storm, even though TDR's own crawl had
//! the real name all along. Keeping TDR's storm identity in TDR's own table
//! (redundant with recon MET's copy, deliberately) makes it immune to that.
//!
//! An admin correction (`edit_mission`) sets `storm_locked`, which tells
//! ingest's upsert (`tdr_ingest.rs::harvest_mission_dir`) to never touch
//! `storm_name`/`storm_id` on that row again — otherwise a re-crawl could
//! silently revert a manual fix.
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

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS missions (
    mission_id   TEXT PRIMARY KEY,
    year         INTEGER NOT NULL,
    aircraft     TEXT,
    tail_num     TEXT,
    -- TDR's own authoritative storm identity — captured at ingest time from
    -- a same-host source (jobfile for Level 1b, path slug for Level 2), or
    -- corrected by hand via the admin console. NULL until something (ingest
    -- or an admin) sets it; reads present that as 'Unknown'.
    storm_name   TEXT,
    storm_id     TEXT,
    -- When set, ingest's upsert must leave storm_name/storm_id alone — an
    -- admin correction that a re-crawl should never silently revert.
    storm_locked INTEGER NOT NULL DEFAULT 0,
    has_level1b  INTEGER NOT NULL DEFAULT 0,
    has_level2   INTEGER NOT NULL DEFAULT 0,
    fetched_at   INTEGER NOT NULL
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
    /// `COALESCE(storm_name, 'Unknown')` — see `MISSION_SELECT`.
    pub storm_name: String,
    pub storm_id: Option<String>,
    /// When true, ingest will never overwrite `storm_name`/`storm_id` on this
    /// row — set automatically by `edit_mission` whenever an admin corrects
    /// either field, unless the caller explicitly says otherwise.
    pub storm_locked: bool,
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
            storm_locked: row.get::<_, i64>("storm_locked")? != 0,
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
    pub id: i64,
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
            id: row.get("id")?,
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
    pub id: i64,
    pub level: String,
    pub start_time: String,
    pub stop_time: String,
    pub source_url: String,
}

impl LegRecord {
    fn from_row(row: &Row) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            level: row.get("level")?,
            start_time: row.get("start_time")?,
            stop_time: row.get("stop_time")?,
            source_url: row.get("source_url")?,
        })
    }
}

/// The WRITE path (startup init + ingest). Opens `db_path`, turns foreign keys
/// on, applies the current `SCHEMA`, and runs the one-time table rebuild(s) for
/// pre-change DBs. This is the only place the schema is created/migrated —
/// other connections deliberately never touch DDL, so `init_db` must run at
/// startup before any `get_connection`/`get_connection_rw` opens.
pub fn init_db(db_path: &Path) -> rusqlite::Result<Connection> {
    let conn = crate::services::db::open(db_path)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.execute_batch(SCHEMA)?;
    migrate_rebuild(&conn)?;
    Ok(conn)
}

/// The READ path used by every public handler. Opens `tdr_db` read-only —
/// storm identity is entirely TDR's own now, so unlike the old join-based
/// design this needs nothing else attached. Runs no DDL/migration/UPDATE and
/// is set `query_only` so it can never write.
pub fn get_connection(tdr_db: &Path) -> rusqlite::Result<Connection> {
    let conn = crate::services::db::open(tdr_db)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "query_only", "ON")?;
    Ok(conn)
}

/// The WRITE path for the admin editor (create/edit/delete). Mirrors
/// `tokens::get_connection` — opens, turns on foreign keys, and re-applies the
/// (idempotent) `SCHEMA`; migration only ever needs to run once, at startup
/// via `init_db`, so it's deliberately not repeated here.
pub fn get_connection_rw(tdr_db: &Path) -> rusqlite::Result<Connection> {
    let conn = crate::services::db::open(tdr_db)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

/// One-time rebuild(s) of the `missions` table for DBs created under an older
/// schema. Runs in two independent stages, each a no-op if its marker column
/// isn't present, so an ancient DB migrates straight through both in one call:
///
/// 1. The original denormalized `storm_name` + `storm_source`/`pending`
///    reconciliation columns → folds into `storm_label`.
/// 2. The `storm_label`-as-fallback shape (the join-based design) →
///    `storm_label` renamed to `storm_name` (now authoritative), plus a new
///    `storm_locked` column defaulting to 0.
///
/// Both stages use SQLite's recommended table-rebuild dance: copy into a fresh
/// table on the new schema, drop the old one, rename. Foreign keys are turned
/// off around each rebuild (the `files`/`legs` FKs reference
/// `missions(mission_id)`), then back on. A fresh DB created straight from
/// `SCHEMA` has neither marker column and is left untouched.
fn migrate_rebuild(conn: &Connection) -> rusqlite::Result<()> {
    migrate_ancient_to_storm_label(conn)?;
    migrate_storm_label_to_storm_name(conn)?;
    Ok(())
}

fn table_columns(conn: &Connection, table: &str) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let cols = stmt.query_map([], |r| r.get::<_, String>(1))?.collect::<Result<_, _>>()?;
    Ok(cols)
}

fn migrate_ancient_to_storm_label(conn: &Connection) -> rusqlite::Result<()> {
    let cols = table_columns(conn, "missions")?;
    let needs_rebuild = cols.iter().any(|c| c == "storm_name" || c == "storm_source" || c == "pending");
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

fn migrate_storm_label_to_storm_name(conn: &Connection) -> rusqlite::Result<()> {
    let cols = table_columns(conn, "missions")?;
    if !cols.iter().any(|c| c == "storm_label") {
        return Ok(());
    }

    conn.pragma_update(None, "foreign_keys", "OFF")?;
    let res = conn.execute_batch(
        "BEGIN;
         CREATE TABLE missions_new (
             mission_id   TEXT PRIMARY KEY,
             year         INTEGER NOT NULL,
             aircraft     TEXT,
             tail_num     TEXT,
             storm_name   TEXT,
             storm_id     TEXT,
             storm_locked INTEGER NOT NULL DEFAULT 0,
             has_level1b  INTEGER NOT NULL DEFAULT 0,
             has_level2   INTEGER NOT NULL DEFAULT 0,
             fetched_at   INTEGER NOT NULL
         );
         INSERT INTO missions_new
             (mission_id, year, aircraft, tail_num, storm_name, storm_id, storm_locked, has_level1b, has_level2, fetched_at)
         SELECT mission_id, year, aircraft, tail_num, storm_label, storm_id, 0,
                has_level1b, has_level2, fetched_at
         FROM missions;
         DROP TABLE missions;
         ALTER TABLE missions_new RENAME TO missions;
         CREATE INDEX IF NOT EXISTS idx_tdr_missions_year ON missions(year);
         COMMIT;",
    );
    if res.is_err() {
        let _ = conn.execute_batch("ROLLBACK");
    }
    conn.pragma_update(None, "foreign_keys", "ON")?;
    res
}

/// The mission SELECT shared by every mission read. `storm_name` is now a
/// plain stored column — no join, no fallback chain beyond `NULL -> 'Unknown'`.
const MISSION_SELECT: &str = "\
    SELECT mission_id, year, aircraft, tail_num, \
           COALESCE(storm_name, 'Unknown') AS storm_name, storm_id, storm_locked, \
           has_level1b, has_level2 \
    FROM missions";

pub fn list_years(conn: &Connection) -> rusqlite::Result<Vec<i64>> {
    let mut stmt = conn.prepare("SELECT DISTINCT year FROM missions ORDER BY year")?;
    let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
    rows.collect()
}

pub fn list_storms_for_year(conn: &Connection, year: i64) -> rusqlite::Result<Vec<StormSummary>> {
    let mut stmt = conn.prepare(
        "SELECT COALESCE(storm_name, 'Unknown') AS storm_name, \
                COUNT(*) AS mission_count, MAX(storm_id) AS storm_id \
         FROM missions WHERE year = ?1 GROUP BY storm_name ORDER BY storm_name",
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
    let sql = format!(
        "{MISSION_SELECT} \
         WHERE year = ?1 AND COALESCE(storm_name, 'Unknown') = ?2 COLLATE NOCASE \
         ORDER BY mission_id"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params![year, storm_name], Mission::from_row)?;
    rows.collect()
}

pub fn get_mission(conn: &Connection, mission_id: &str) -> rusqlite::Result<Option<Mission>> {
    conn.query_row(&format!("{MISSION_SELECT} WHERE mission_id = ?1"), [mission_id], Mission::from_row)
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

// ── Admin write path (create/edit/delete) ──────────────────────────────────

/// Fields for a hand-entered mission — for the case where ingest never saw a
/// mission (or its source data is gone) but an admin knows it should exist.
#[derive(Debug, Default)]
pub struct NewMission<'a> {
    pub mission_id: &'a str,
    pub year: i64,
    pub aircraft: Option<&'a str>,
    pub tail_num: Option<&'a str>,
    pub storm_name: Option<&'a str>,
    pub storm_id: Option<&'a str>,
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn create_mission(conn: &Connection, new: &NewMission) -> rusqlite::Result<Mission> {
    let now = now_unix();
    conn.execute(
        "INSERT INTO missions \
         (mission_id, year, aircraft, tail_num, storm_name, storm_id, storm_locked, has_level1b, has_level2, fetched_at) \
         VALUES (?1,?2,?3,?4,?5,?6,1,0,0,?7)",
        rusqlite::params![new.mission_id, new.year, new.aircraft, new.tail_num, new.storm_name, new.storm_id, now],
    )?;
    Ok(get_mission(conn, new.mission_id)?.expect("just inserted"))
}

/// Field-by-field edit — `None` means "leave unchanged" for every field
/// except `storm_id`, where `Some(None)` explicitly clears it to `NULL` (an
/// empty string from the console maps to that) and `None` leaves it alone.
#[derive(Debug, Default)]
pub struct EditMission<'a> {
    pub aircraft: Option<&'a str>,
    pub tail_num: Option<&'a str>,
    pub storm_name: Option<&'a str>,
    pub storm_id: Option<Option<&'a str>>,
    pub has_level1b: Option<bool>,
    pub has_level2: Option<bool>,
    /// `None` defers to the auto-lock rule (see module doc); `Some(_)` always
    /// wins, including handing a row back to ingest with `Some(false)`.
    pub storm_locked: Option<bool>,
}

pub fn edit_mission(
    conn: &Connection,
    mission_id: &str,
    edit: &EditMission,
) -> rusqlite::Result<Option<Mission>> {
    if get_mission(conn, mission_id)?.is_none() {
        return Ok(None);
    }

    let locked = edit
        .storm_locked
        .unwrap_or(edit.storm_name.is_some() || edit.storm_id.is_some());

    if let Some(v) = edit.aircraft {
        conn.execute("UPDATE missions SET aircraft = ?1 WHERE mission_id = ?2", rusqlite::params![v, mission_id])?;
    }
    if let Some(v) = edit.tail_num {
        conn.execute("UPDATE missions SET tail_num = ?1 WHERE mission_id = ?2", rusqlite::params![v, mission_id])?;
    }
    if let Some(v) = edit.storm_name {
        conn.execute("UPDATE missions SET storm_name = ?1 WHERE mission_id = ?2", rusqlite::params![v, mission_id])?;
    }
    if let Some(v) = edit.storm_id {
        conn.execute("UPDATE missions SET storm_id = ?1 WHERE mission_id = ?2", rusqlite::params![v, mission_id])?;
    }
    if let Some(v) = edit.has_level1b {
        conn.execute(
            "UPDATE missions SET has_level1b = ?1 WHERE mission_id = ?2",
            rusqlite::params![v as i64, mission_id],
        )?;
    }
    if let Some(v) = edit.has_level2 {
        conn.execute(
            "UPDATE missions SET has_level2 = ?1 WHERE mission_id = ?2",
            rusqlite::params![v as i64, mission_id],
        )?;
    }
    conn.execute(
        "UPDATE missions SET storm_locked = ?1 WHERE mission_id = ?2",
        rusqlite::params![locked as i64, mission_id],
    )?;

    get_mission(conn, mission_id)
}

/// Deletes a mission; `files`/`legs` cascade via `ON DELETE CASCADE`.
pub fn delete_mission(conn: &Connection, mission_id: &str) -> rusqlite::Result<bool> {
    Ok(conn.execute("DELETE FROM missions WHERE mission_id = ?1", [mission_id])? > 0)
}

pub fn delete_file(conn: &Connection, file_id: i64) -> rusqlite::Result<bool> {
    Ok(conn.execute("DELETE FROM files WHERE id = ?1", [file_id])? > 0)
}

pub fn delete_leg(conn: &Connection, leg_id: i64) -> rusqlite::Result<bool> {
    Ok(conn.execute("DELETE FROM legs WHERE id = ?1", [leg_id])? > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn
    }

    fn seed_mission(conn: &Connection, mission_id: &str, storm_name: Option<&str>) {
        conn.execute(
            "INSERT INTO missions (mission_id, year, storm_name, storm_id, storm_locked, fetched_at) \
             VALUES (?1, ?2, ?3, NULL, 0, 0)",
            rusqlite::params![mission_id, 2026, storm_name],
        )
        .unwrap();
    }

    #[test]
    fn migrates_ancient_schema_through_both_stages() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE missions (
                mission_id TEXT PRIMARY KEY, year INTEGER NOT NULL, aircraft TEXT, tail_num TEXT,
                storm_name TEXT NOT NULL, storm_source TEXT, pending INTEGER, storm_id TEXT,
                has_level1b INTEGER NOT NULL DEFAULT 0, has_level2 INTEGER NOT NULL DEFAULT 0,
                fetched_at INTEGER NOT NULL
            );
             INSERT INTO missions (mission_id, year, storm_name, storm_id, has_level1b, has_level2, fetched_at)
             VALUES ('20260616H1', 2026, 'Fausto', 'EP062026', 1, 0, 100);",
        )
        .unwrap();

        migrate_rebuild(&conn).unwrap();

        let cols = table_columns(&conn, "missions").unwrap();
        assert!(cols.contains(&"storm_name".to_string()));
        assert!(cols.contains(&"storm_locked".to_string()));
        assert!(!cols.contains(&"storm_label".to_string()));
        assert!(!cols.contains(&"storm_source".to_string()));

        let m = get_mission(&conn, "20260616H1").unwrap().unwrap();
        assert_eq!(m.storm_name, "Fausto");
        assert_eq!(m.storm_id.as_deref(), Some("EP062026"));
        assert!(!m.storm_locked);
    }

    #[test]
    fn migrates_storm_label_shape() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE missions (
                mission_id TEXT PRIMARY KEY, year INTEGER NOT NULL, aircraft TEXT, tail_num TEXT,
                storm_label TEXT, storm_id TEXT,
                has_level1b INTEGER NOT NULL DEFAULT 0, has_level2 INTEGER NOT NULL DEFAULT 0,
                fetched_at INTEGER NOT NULL
            );
             INSERT INTO missions (mission_id, year, storm_label, storm_id, has_level1b, has_level2, fetched_at)
             VALUES ('20260616H1', 2026, 'Fausto', 'EP062026', 1, 0, 100);",
        )
        .unwrap();

        migrate_rebuild(&conn).unwrap();
        let m = get_mission(&conn, "20260616H1").unwrap().unwrap();
        assert_eq!(m.storm_name, "Fausto");
        assert!(!m.storm_locked);
    }

    #[test]
    fn create_edit_delete_mission() {
        let conn = mem_conn();
        let created = create_mission(
            &conn,
            &NewMission { mission_id: "20260701H1", year: 2026, storm_name: Some("Fausto"), ..Default::default() },
        )
        .unwrap();
        assert_eq!(created.storm_name, "Fausto");
        assert!(created.storm_locked, "hand-created missions start locked");

        let edited = edit_mission(
            &conn,
            "20260701H1",
            &EditMission { aircraft: Some("NOAA 42 (Kermit)"), ..Default::default() },
        )
        .unwrap()
        .unwrap();
        assert_eq!(edited.aircraft.as_deref(), Some("NOAA 42 (Kermit)"));
        assert_eq!(edited.storm_name, "Fausto", "unrelated edit doesn't touch storm_name");

        assert!(delete_mission(&conn, "20260701H1").unwrap());
        assert!(get_mission(&conn, "20260701H1").unwrap().is_none());
        assert!(!delete_mission(&conn, "20260701H1").unwrap());
    }

    #[test]
    fn editing_storm_name_auto_locks_unless_overridden() {
        let conn = mem_conn();
        seed_mission(&conn, "20260616H1", None);

        let m = edit_mission(&conn, "20260616H1", &EditMission { storm_name: Some("Fausto"), ..Default::default() })
            .unwrap()
            .unwrap();
        assert!(m.storm_locked, "correcting storm_name should auto-lock");

        let m = edit_mission(
            &conn,
            "20260616H1",
            &EditMission { aircraft: Some("N42"), storm_locked: Some(false), ..Default::default() },
        )
        .unwrap()
        .unwrap();
        assert!(!m.storm_locked, "explicit storm_locked always wins");
        assert_eq!(m.storm_name, "Fausto", "clearing the lock doesn't touch the name itself");
    }

    #[test]
    fn clearing_storm_id_sets_null() {
        let conn = mem_conn();
        conn.execute(
            "INSERT INTO missions (mission_id, year, storm_name, storm_id, storm_locked, fetched_at) \
             VALUES ('20260616H1', 2026, 'Fausto', 'EP062026', 0, 0)",
            [],
        )
        .unwrap();

        let m = edit_mission(&conn, "20260616H1", &EditMission { storm_id: Some(None), ..Default::default() })
            .unwrap()
            .unwrap();
        assert_eq!(m.storm_id, None);
    }

    #[test]
    fn delete_file_and_leg() {
        let conn = mem_conn();
        seed_mission(&conn, "20260616H1", Some("Fausto"));
        conn.execute(
            "INSERT INTO files (mission_id, level, product, format, analysis_time, source_url, fetched_at) \
             VALUES ('20260616H1', '1b', 'xy', 'nc', '1200', 'https://example/x.nc', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO legs (mission_id, level, start_time, stop_time, source_url, fetched_at) \
             VALUES ('20260616H1', '1b', '1100', '1300', 'https://example/x.tar', 0)",
            [],
        )
        .unwrap();

        let file_id: i64 = conn.query_row("SELECT id FROM files", [], |r| r.get(0)).unwrap();
        let leg_id: i64 = conn.query_row("SELECT id FROM legs", [], |r| r.get(0)).unwrap();
        assert!(delete_file(&conn, file_id).unwrap());
        assert!(delete_leg(&conn, leg_id).unwrap());
        assert!(!delete_file(&conn, file_id).unwrap());
    }

    #[test]
    fn mission_delete_cascades_files_and_legs() {
        let conn = mem_conn();
        seed_mission(&conn, "20260616H1", Some("Fausto"));
        conn.execute(
            "INSERT INTO files (mission_id, level, product, format, analysis_time, source_url, fetched_at) \
             VALUES ('20260616H1', '1b', 'xy', 'nc', '1200', 'https://example/x.nc', 0)",
            [],
        )
        .unwrap();

        assert!(delete_mission(&conn, "20260616H1").unwrap());
        let remaining: i64 = conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0)).unwrap();
        assert_eq!(remaining, 0);
    }
}
