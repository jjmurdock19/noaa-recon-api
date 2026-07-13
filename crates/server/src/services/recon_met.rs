//! Recon MET (1-second flight-level obs) archive — READ path, port of the query
//! helpers in `app/services/recon_met.py`.
//!
//! As with storms.rs, the crawler/ingest (`harvest_mission`, `run_ingest`, the
//! reconcilers) is NOT ported yet — the shared `data/recon_met.sqlite` is
//! populated by the existing Python `scripts/ingest_recon_met.py`. This module
//! is what `app/routers/recon.py` needs to serve.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Row};
use serde::Serialize;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS missions (
    mission_id  TEXT PRIMARY KEY,
    year        INTEGER NOT NULL,
    storm_name  TEXT NOT NULL,
    storm_id    TEXT,
    aircraft    TEXT,
    tail_num    TEXT,
    flight_date TEXT,
    start_unix  INTEGER,
    end_unix    INTEGER,
    nc_version  TEXT,
    source_url  TEXT,
    lat_min     REAL,
    lat_max     REAL,
    lon_min     REAL,
    lon_max     REAL,
    obs_count   INTEGER DEFAULT 0,
    fetched_at  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_recon_missions_year ON missions(year);
CREATE INDEX IF NOT EXISTS idx_recon_missions_year_storm ON missions(year, storm_name);

CREATE TABLE IF NOT EXISTS observations (
    id          INTEGER PRIMARY KEY,
    mission_id  TEXT NOT NULL REFERENCES missions(mission_id) ON DELETE CASCADE,
    seq_num     INTEGER NOT NULL,
    unix_time   INTEGER NOT NULL,
    lat         REAL NOT NULL,
    lon         REAL NOT NULL,
    wind_kt     REAL,
    wind_dir    REAL,
    sfmr_kt     REAL,
    alt_m       REAL
);
CREATE INDEX IF NOT EXISTS idx_recon_obs_mission ON observations(mission_id, seq_num);
";

/// A full `missions` row (SELECT *), used by `get_mission`.
#[derive(Debug, Clone, Serialize)]
pub struct Mission {
    pub mission_id: String,
    pub year: i64,
    pub storm_name: String,
    pub storm_id: Option<String>,
    pub aircraft: Option<String>,
    pub tail_num: Option<String>,
    pub flight_date: Option<String>,
    pub start_unix: Option<i64>,
    pub end_unix: Option<i64>,
    pub source_url: Option<String>,
    pub obs_count: i64,
}

impl Mission {
    fn from_row(row: &Row) -> rusqlite::Result<Self> {
        Ok(Self {
            mission_id: row.get("mission_id")?,
            year: row.get("year")?,
            storm_name: row.get("storm_name")?,
            storm_id: row.get("storm_id")?,
            aircraft: row.get("aircraft")?,
            tail_num: row.get("tail_num")?,
            flight_date: row.get("flight_date")?,
            start_unix: row.get("start_unix")?,
            end_unix: row.get("end_unix")?,
            source_url: row.get("source_url")?,
            obs_count: row.get("obs_count")?,
        })
    }
}

/// One decimated observation: [unix_time, lat, lon, wind_kt, wind_dir, sfmr_kt, alt_m].
#[derive(Debug, Clone)]
pub struct Observation {
    pub unix_time: i64,
    pub lat: f64,
    pub lon: f64,
    pub wind_kt: Option<f64>,
    pub wind_dir: Option<f64>,
    pub sfmr_kt: Option<f64>,
    pub alt_m: Option<f64>,
}

/// A `storm_name -> mission_count` roll-up for a year.
#[derive(Debug, Clone, Serialize)]
pub struct StormSummary {
    pub storm_name: String,
    pub storm_id: Option<String>,
    pub mission_count: i64,
}

/// A mission summary row for the storm drill-down.
#[derive(Debug, Clone, Serialize)]
pub struct MissionSummary {
    pub mission_id: String,
    pub aircraft: Option<String>,
    pub tail_num: Option<String>,
    pub flight_date: Option<String>,
    pub start_unix: Option<i64>,
    pub end_unix: Option<i64>,
    pub obs_count: i64,
    pub source_url: Option<String>,
}

pub fn get_connection(db_path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(db_path)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

pub fn list_years(conn: &Connection) -> rusqlite::Result<Vec<i64>> {
    let mut stmt = conn.prepare("SELECT DISTINCT year FROM missions ORDER BY year")?;
    let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
    rows.collect()
}

pub fn list_storms_for_year(conn: &Connection, year: i64) -> rusqlite::Result<Vec<StormSummary>> {
    let mut stmt = conn.prepare(
        "SELECT storm_name, COUNT(*) AS mission_count, MAX(storm_id) AS storm_id \
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
) -> rusqlite::Result<Vec<MissionSummary>> {
    let mut stmt = conn.prepare(
        "SELECT mission_id, aircraft, tail_num, flight_date, start_unix, end_unix, \
         obs_count, source_url FROM missions \
         WHERE year = ?1 AND storm_name = ?2 COLLATE NOCASE ORDER BY start_unix",
    )?;
    let rows = stmt.query_map(rusqlite::params![year, storm_name], |r| {
        Ok(MissionSummary {
            mission_id: r.get("mission_id")?,
            aircraft: r.get("aircraft")?,
            tail_num: r.get("tail_num")?,
            flight_date: r.get("flight_date")?,
            start_unix: r.get("start_unix")?,
            end_unix: r.get("end_unix")?,
            obs_count: r.get("obs_count")?,
            source_url: r.get("source_url")?,
        })
    })?;
    rows.collect()
}

pub fn get_mission(conn: &Connection, mission_id: &str) -> rusqlite::Result<Option<Mission>> {
    conn.query_row(
        "SELECT * FROM missions WHERE mission_id = ?1",
        [mission_id],
        Mission::from_row,
    )
    .optional()
}

pub fn get_observations(conn: &Connection, mission_id: &str) -> rusqlite::Result<Vec<Observation>> {
    let mut stmt = conn.prepare(
        "SELECT unix_time, lat, lon, wind_kt, wind_dir, sfmr_kt, alt_m \
         FROM observations WHERE mission_id = ?1 ORDER BY seq_num",
    )?;
    let rows = stmt.query_map([mission_id], |r| {
        Ok(Observation {
            unix_time: r.get("unix_time")?,
            lat: r.get("lat")?,
            lon: r.get("lon")?,
            wind_kt: r.get("wind_kt")?,
            wind_dir: r.get("wind_dir")?,
            sfmr_kt: r.get("sfmr_kt")?,
            alt_m: r.get("alt_m")?,
        })
    })?;
    rows.collect()
}
