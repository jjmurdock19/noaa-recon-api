"""Historical storm track database. Two sources, stitched together so the
archive stays current up to the present day:

  1. HURDAT2 — NOAA/NHC's official reconciled best-track archive. The
     authoritative source, but only republished once a year, months after
     each season closes (Atlantic file below covers through 2024; the
     Pacific one through 2023).
       - Atlantic:            https://www.nhc.noaa.gov/data/hurdat/hurdat2-1851-2024-040425.txt
       - East/Central Pacific: https://www.nhc.noaa.gov/data/hurdat/hurdat2-nepac-1949-2023-042624.txt
  2. ATCF b-decks — NHC's operational best-track feed, one file per storm,
     updated in near-real-time all season (https://ftp.nhc.noaa.gov/atcf/btk/)
     and archived by season once it ends (https://ftp.nhc.noaa.gov/atcf/archive/{year}/,
     gzipped). This fills the gap between HURDAT2's last reconciled season
     and today, and is what the nightly ingest re-fetches (see
     scripts/ingest_storms.py) to keep the archive current.

Why HURDAT2/ATCF, not TC-Atlas: TC-Atlas's own metadata (proxied elsewhere
in this repo for TDR radar, see app/routers/tdr.py and the hurricanes
site's tdr-archive.js) only has position fixes at moments a recon aircraft
actually flew a mission — 1,510 cases, 1997-2024. That can't answer "what
was this storm doing at 3am on a date nobody flew" or cover a storm still
in progress.

Named storms only: both formats also carry every tropical depression that
never got a name (HURDAT2 labels these "UNNAMED"; ATCF just never assigns
a real STORMNAME, leaving a placeholder like an invest ID or an ordinal
word like "ONE"). Those are filtered out everywhere in this module via
is_real_storm_name() — as a side effect this also drops HURDAT2's
19th/early-20th-century tail, since Atlantic storms didn't get real names
until 1950.

Format reference — HURDAT2 (header line + N data lines per storm):
  Header: "AL092023,             LEE,     47,"           -> basin+num+year, name, entry count
  Data:   "20230905, 0000,  , TD, 13.4N,  38.7W,  30, 1006, ...12 wind-radii fields..., rmw"

Format reference — ATCF b-deck (one row per synoptic hour, one file per storm):
  "AL, 01, 2026061800,   , BEST,   0, 294N,  948W,  35,  999, TS,  34, NEQ, ...many more fields..., ARTHUR, S, ..."
  Fields used here (0-indexed after comma-split + strip): 2=YYYYMMDDHH,
  4=TECH (only "BEST" rows are real fixes), 6=lat, 7=lon (tenths of a
  degree, no decimal point — "235N" = 23.5N), 8=VMAX (kt), 9=MSLP (mb, 0 =
  unknown), 10=status code, 27=STORMNAME (blank/placeholder until named).

Both formats only give us the fields this project's schema asks for
(datetime, lat/lon, intensity category, wind, pressure) — wind radii, RMW,
etc. exist in both sources but aren't modeled here.
"""
import datetime
import gzip
import re
import sqlite3
from pathlib import Path
from typing import Optional

import httpx

from app.paths import STORMS_DB_PATH

HURDAT_URLS = {
    "AL": "https://www.nhc.noaa.gov/data/hurdat/hurdat2-1851-2024-040425.txt",
    "EP": "https://www.nhc.noaa.gov/data/hurdat/hurdat2-nepac-1949-2023-042624.txt",
}

# Basins to extend past HURDAT2's coverage with ATCF b-deck data. EP and CP
# are reported together in one HURDAT2 file but as separate ATCF basins.
ATCF_BASIN_PREFIX = {"AL": "al", "EP": "ep", "CP": "cp"}
ATCF_ARCHIVE_DIR_URL = "https://ftp.nhc.noaa.gov/atcf/archive/{year}/"
ATCF_ARCHIVE_FILE_URL = "https://ftp.nhc.noaa.gov/atcf/archive/{year}/{filename}"
ATCF_BTK_DIR_URL = "https://ftp.nhc.noaa.gov/atcf/btk/"
ATCF_BTK_FILE_URL = "https://ftp.nhc.noaa.gov/atcf/btk/{filename}"

_HEADER_RE = re.compile(r"^([A-Z]{2})(\d{2})(\d{4}),\s*([^,]+?)\s*,\s*(\d+)\s*,?\s*$")

_STATUS_LABELS = {
    "TD": "Tropical Depression",
    "TS": "Tropical Storm",
    "SD": "Subtropical Depression",
    "SS": "Subtropical Storm",
    "EX": "Extratropical Cyclone",
    "LO": "Low",
    "WV": "Tropical Wave",
    "DB": "Disturbance",
}

# Placeholder names a storm carries before (or instead of) being named.
# ATCF ordinal words count up far higher than any season realistically
# reaches; 30 is a generous ceiling.
_ORDINAL_WORDS = {
    "ONE", "TWO", "THREE", "FOUR", "FIVE", "SIX", "SEVEN", "EIGHT", "NINE", "TEN",
    "ELEVEN", "TWELVE", "THIRTEEN", "FOURTEEN", "FIFTEEN", "SIXTEEN", "SEVENTEEN",
    "EIGHTEEN", "NINETEEN", "TWENTY", "TWENTYONE", "TWENTYTWO", "TWENTYTHREE",
    "TWENTYFOUR", "TWENTYFIVE", "TWENTYSIX", "TWENTYSEVEN", "TWENTYEIGHT",
    "TWENTYNINE", "THIRTY",
}
_NAME_BLOCKLIST = {"", "UNNAMED", "INVEST", "NONAME"} | _ORDINAL_WORDS


def is_real_storm_name(name: str) -> bool:
    """False for depressions/invests that never got an actual name — the
    filter that keeps this database to named storms only."""
    n = (name or "").strip().upper()
    if n in _NAME_BLOCKLIST:
        return False
    if n.startswith("GENESIS"):
        return False
    return True


def category_label(status: str, wind_kt: Optional[int]) -> str:
    """Saffir-Simpson category for hurricanes; a plain status label otherwise."""
    if status == "HU" and wind_kt is not None:
        if wind_kt >= 137:
            return "Category 5"
        if wind_kt >= 113:
            return "Category 4"
        if wind_kt >= 96:
            return "Category 3"
        if wind_kt >= 83:
            return "Category 2"
        if wind_kt >= 64:
            return "Category 1"
    return _STATUS_LABELS.get(status, status)


def _parse_latlon(lat_raw: str, lon_raw: str) -> tuple[float, float]:
    lat = float(lat_raw[:-1])
    if lat_raw[-1] == "S":
        lat = -lat
    lon = float(lon_raw[:-1])
    if lon_raw[-1] == "W":
        lon = -lon
    return lat, lon


def parse_hurdat2(text: str):
    """Yields (basin, atcf_id, year, name, [point dicts]) per storm."""
    lines = [ln.strip() for ln in text.splitlines() if ln.strip()]
    i = 0
    while i < len(lines):
        m = _HEADER_RE.match(lines[i])
        if not m:
            raise ValueError(f"Expected a HURDAT2 header line, got: {lines[i]!r}")
        basin, num, year, name, count = m.group(1), m.group(2), int(m.group(3)), m.group(4), int(m.group(5))
        atcf_id = f"{basin}{num}{year}"
        points = []
        for j in range(count):
            fields = [f.strip() for f in lines[i + 1 + j].split(",")]
            date_raw, time_raw, _record_id, status = fields[0], fields[1], fields[2], fields[3]
            lat, lon = _parse_latlon(fields[4], fields[5])
            wind_kt = int(fields[6])
            if wind_kt < 0:
                wind_kt = None
            pressure_mb = int(fields[7])
            if pressure_mb < 0:
                pressure_mb = None
            dt = datetime.datetime.strptime(date_raw + time_raw, "%Y%m%d%H%M").replace(
                tzinfo=datetime.timezone.utc
            )
            points.append({
                "datetime_utc": dt.strftime("%Y-%m-%dT%H:%M:%SZ"),
                "status": status,
                "category": category_label(status, wind_kt),
                "lat": lat,
                "lon": lon,
                "wind_kt": wind_kt,
                "pressure_mb": pressure_mb,
            })
        yield basin, atcf_id, year, name, points
        i += 1 + count


def _parse_atcf_latlon(raw: str) -> float:
    val = float(raw[:-1]) / 10.0
    if raw[-1] in ("S", "W"):
        val = -val
    return val


def parse_atcf_bdeck(text: str, basin: str, num: str, year: int):
    """Parses one ATCF b-deck file (every BEST-track row for one storm
    number). Returns (atcf_id, name, [point dicts]), or None if the storm
    never got a real name (still an invest / unnamed depression)."""
    points_by_dt: dict[datetime.datetime, dict] = {}
    final_name = ""
    for line in text.splitlines():
        line = line.strip()
        if not line:
            continue
        fields = [f.strip() for f in line.split(",")]
        if len(fields) < 28 or fields[4] != "BEST":
            continue
        if fields[27]:
            final_name = fields[27]
        dt = datetime.datetime.strptime(fields[2], "%Y%m%d%H").replace(tzinfo=datetime.timezone.utc)
        lat = _parse_atcf_latlon(fields[6])
        lon = _parse_atcf_latlon(fields[7])
        wind_kt = int(fields[8]) if fields[8] else None
        if wind_kt is not None and wind_kt <= 0:
            wind_kt = None
        pressure_mb = int(fields[9]) if fields[9] else None
        if pressure_mb is not None and pressure_mb <= 0:
            pressure_mb = None
        status = fields[10]
        # Multiple TECH rows (BEST + various model runs) can share a synoptic
        # hour; last BEST row wins, consistent with ATCF's own "latest write"
        # convention for the b-deck file.
        points_by_dt[dt] = {
            "datetime_utc": dt.strftime("%Y-%m-%dT%H:%M:%SZ"),
            "status": status,
            "category": category_label(status, wind_kt),
            "lat": lat,
            "lon": lon,
            "wind_kt": wind_kt,
            "pressure_mb": pressure_mb,
        }

    if not is_real_storm_name(final_name):
        return None
    atcf_id = f"{basin}{num}{year}"
    points = [points_by_dt[dt] for dt in sorted(points_by_dt)]
    if not points:
        return None
    return atcf_id, final_name.upper(), points


def list_atcf_filenames(dir_html: str, basin_prefix: str, year: int, gz: bool) -> list[str]:
    """Extracts b-deck filenames for one basin/year from a directory listing
    page's raw HTML (works for both the archive/{year}/ and btk/ listings)."""
    suffix = ".dat.gz" if gz else ".dat"
    pattern = re.compile(rf'href="(b{basin_prefix}\d{{2}}{year}{re.escape(suffix)})"')
    return sorted(set(pattern.findall(dir_html)))


SCHEMA = """
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
"""


def get_connection() -> sqlite3.Connection:
    conn = sqlite3.connect(str(STORMS_DB_PATH))
    conn.row_factory = sqlite3.Row
    conn.execute("PRAGMA foreign_keys = ON")
    conn.executescript(SCHEMA)
    return conn


def _upsert_storm(conn: sqlite3.Connection, basin: str, atcf_id: str, year: int, name: str, points: list[dict]) -> None:
    conn.execute(
        "INSERT INTO storms (basin, atcf_id, year, name) VALUES (?, ?, ?, ?) "
        "ON CONFLICT(atcf_id) DO UPDATE SET basin=excluded.basin, year=excluded.year, name=excluded.name",
        (basin, atcf_id, year, name),
    )
    storm_id = conn.execute("SELECT id FROM storms WHERE atcf_id = ?", (atcf_id,)).fetchone()["id"]
    conn.execute("DELETE FROM track_points WHERE storm_id = ?", (storm_id,))
    conn.executemany(
        "INSERT INTO track_points (storm_id, datetime_utc, status, category, lat, lon, wind_kt, pressure_mb) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        [
            (storm_id, p["datetime_utc"], p["status"], p["category"], p["lat"], p["lon"], p["wind_kt"], p["pressure_mb"])
            for p in points
        ],
    )


def ingest_basin(conn: sqlite3.Connection, basin_file_text: str) -> int:
    """Parses one HURDAT2 file and upserts its named storms/track points
    (unnamed depressions are skipped). Returns the number ingested."""
    count = 0
    for basin, atcf_id, year, name, points in parse_hurdat2(basin_file_text):
        if not is_real_storm_name(name):
            continue
        _upsert_storm(conn, basin, atcf_id, year, name, points)
        count += 1
    conn.commit()
    return count


def ingest_atcf_storm(conn: sqlite3.Connection, basin: str, num: str, year: int, text: str) -> bool:
    """Parses one ATCF b-deck file and upserts it if the storm got a real
    name. Returns whether it was ingested."""
    parsed = parse_atcf_bdeck(text, basin, num, year)
    if parsed is None:
        return False
    atcf_id, name, points = parsed
    _upsert_storm(conn, basin, atcf_id, year, name, points)
    conn.commit()
    return True


def max_year_for_basin(conn: sqlite3.Connection, basin: str) -> Optional[int]:
    row = conn.execute("SELECT MAX(year) AS y FROM storms WHERE basin = ?", (basin,)).fetchone()
    return row["y"] if row and row["y"] is not None else None


# ── Query helpers used by app/routers/storms.py ─────────────────────────
def list_years(conn: sqlite3.Connection) -> list[int]:
    rows = conn.execute("SELECT DISTINCT year FROM storms ORDER BY year").fetchall()
    return [r["year"] for r in rows]


def list_storms_for_year(conn: sqlite3.Connection, year: int) -> list[sqlite3.Row]:
    return conn.execute(
        "SELECT atcf_id, basin, year, name FROM storms WHERE year = ? ORDER BY name", (year,)
    ).fetchall()


def find_storms(conn: sqlite3.Connection, year: int, name: str, basin: Optional[str] = None) -> list[sqlite3.Row]:
    if basin:
        return conn.execute(
            "SELECT id, atcf_id, basin, year, name FROM storms WHERE year = ? AND name = ? AND basin = ?",
            (year, name.upper(), basin.upper()),
        ).fetchall()
    return conn.execute(
        "SELECT id, atcf_id, basin, year, name FROM storms WHERE year = ? AND name = ?",
        (year, name.upper()),
    ).fetchall()


def get_track(conn: sqlite3.Connection, storm_id: int) -> list[sqlite3.Row]:
    return conn.execute(
        "SELECT datetime_utc, status, category, lat, lon, wind_kt, pressure_mb "
        "FROM track_points WHERE storm_id = ? ORDER BY datetime_utc",
        (storm_id,),
    ).fetchall()


def find_nearest_point(conn: sqlite3.Connection, storm_id: int, target_datetime_utc: str) -> Optional[sqlite3.Row]:
    return conn.execute(
        "SELECT datetime_utc, status, category, lat, lon, wind_kt, pressure_mb "
        "FROM track_points WHERE storm_id = ? "
        "ORDER BY ABS(julianday(datetime_utc) - julianday(?)) LIMIT 1",
        (storm_id, target_datetime_utc),
    ).fetchone()


# ── Full ingest orchestration — used by both scripts/ingest_storms.py
# (CLI / nightly systemd timer) and the admin console's "force update"
# button (app/routers/admin.py), so there's exactly one place this pipeline
# is defined ─────────────────────────────────────────────────────────────
def _ingest_atcf_season(client: httpx.Client, conn: sqlite3.Connection, basin: str, year: int, live: bool) -> int:
    """Ingests one basin/year of ATCF b-decks (archived+gzipped for a
    closed season, live+plaintext for the current one). Returns the number
    of named storms ingested."""
    prefix = ATCF_BASIN_PREFIX[basin]
    if live:
        dir_url = ATCF_BTK_DIR_URL
        file_url_tmpl = ATCF_BTK_FILE_URL
    else:
        dir_url = ATCF_ARCHIVE_DIR_URL.format(year=year)
        file_url_tmpl = ATCF_ARCHIVE_FILE_URL

    resp = client.get(dir_url)
    if resp.status_code == 404:
        return 0
    resp.raise_for_status()
    filenames = list_atcf_filenames(resp.text, prefix, year, gz=not live)

    ingested = 0
    for filename in filenames:
        num = filename[len(f"b{prefix}"):len(f"b{prefix}") + 2]
        file_url = file_url_tmpl.format(year=year, filename=filename)
        r = client.get(file_url)
        if r.status_code == 404:
            continue
        r.raise_for_status()
        text = gzip.decompress(r.content).decode("utf-8", errors="replace") if not live else r.text
        if ingest_atcf_storm(conn, basin, num, year, text):
            ingested += 1
    return ingested


def run_ingest() -> dict:
    """Full HURDAT2 + ATCF ingest pass (see module docstring). Returns a
    JSON-serializable summary; safe to call repeatedly (nightly timer, a
    manual CLI run, or the admin console's force-update button all share
    this one code path)."""
    conn = get_connection()
    current_year = datetime.datetime.now(datetime.timezone.utc).year
    summary: dict = {"hurdat2": {}, "atcf": {}, "total_storms": 0}

    with httpx.Client(timeout=60.0, follow_redirects=True) as client:
        for basin, url in HURDAT_URLS.items():
            resp = client.get(url)
            resp.raise_for_status()
            summary["hurdat2"][basin] = ingest_basin(conn, resp.text)

        for basin in ATCF_BASIN_PREFIX:
            start_year = (max_year_for_basin(conn, basin) or (current_year - 1)) + 1
            basin_summary = {}
            for gap_year in range(start_year, current_year):
                basin_summary[str(gap_year)] = _ingest_atcf_season(client, conn, basin, gap_year, live=False)
            basin_summary[str(current_year)] = _ingest_atcf_season(client, conn, basin, current_year, live=True)
            summary["atcf"][basin] = basin_summary

    summary["total_storms"] = conn.execute("SELECT COUNT(*) FROM storms").fetchone()[0]
    conn.close()
    return summary
