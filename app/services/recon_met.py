"""Recon MET (1-second flight-level observation) archive — crawls NOAA's
raw aircraft data archive, decimates and stores each mission's track, and
answers year/storm/mission-id lookups for app/routers/recon.py.

Ported from the hurricanes site's standalone met_harvester.py (same crawl
target, same decimated schema) so this project owns the feature and can be
deployed elsewhere without depending on files living in a sibling repo —
see scripts/ingest_recon_met.py for the harvest entry point and the
README's "Recon MET archive" section for a from-scratch deployment.

Source: https://seb.omao.noaa.gov/pub/acdata/{year}/MET/{mission_id}/ — one
folder per mission, containing a QC'd NetCDF file ({mission_id}_{letter}.nc,
A < B < C..., highest letter wins), a mission summary PDF (storm name is
scraped from it), and an `aampsrc` config file (tail number). No manifest —
each year's mission-folder list has to be crawled from the directory
listing.

Every mission's original NetCDF download URL is kept (`source_url`) so the
API can point a client straight at NOAA's file instead of always serving
our decimated copy — see get_mission()'s "download" affordance in the
router.
"""
import datetime
import io
import logging
import math
import os
import re
import sqlite3
import tempfile
import time
from html.parser import HTMLParser
from pathlib import Path
from typing import Dict, List, Optional, Tuple

import httpx
import netCDF4
import numpy as np
import pypdf

from app.paths import RECON_MET_DB_PATH
from app.services.netcdf_lock import NC_LOCK

log = logging.getLogger(__name__)

BASE_URL = "https://seb.omao.noaa.gov/pub/acdata"
FIRST_YEAR = 2011
DECIMATION = 5  # store every Nth 1-second observation (5 -> 0.2 Hz)
HTTP_TIMEOUT = 30.0
NC_TIMEOUT = 120.0

TAILNUM_MAP = {
    "n42": "NOAA 42 (Kermit)",
    "n43": "NOAA 43 (Miss Piggy)",
    "n49": "NOAA 49 (Gonzo)",
    "n57": "NOAA G-IV",
    "n67": "NOAA G-IV",
}

# Bucket name for flights that either never got a real storm name (training,
# calibration, instrument-test missions) or had one stripped by
# reconcile_mismatched_storm_names() below because their flown track never
# actually came near the storm they were labeled with. "Unknown / Training"
# was the name used before this rename; LEGACY_TRAINING_BUCKET_NAMES lets
# rename_legacy_training_bucket() fold pre-rename archives forward onto the
# current name instead of splitting into two buckets.
TRAINING_BUCKET_NAME = "Training Flights / Research"
LEGACY_TRAINING_BUCKET_NAMES = ("Unknown / Training",)

# Known-bad mission_id -> storm_name corrections that reconcile_mismatched_
# storm_names() can't safely catch on its own. That check confirms a named
# mission by distance-to-track alone, which can't distinguish "this P-3
# flight legitimately never penetrated the storm" (true of plenty of real
# missions — periphery dropsonde work, IFEX research legs, aborted/weather-
# diverted attempts) from "this P-3 flight's own NOAA paperwork was simply
# coded wrong". Tightening the distance threshold to catch the latter was
# evaluated and rejected: simulated against the archive, it flagged ~114
# other P-3 missions sitting in the same 150-500km band as this one, the
# large majority almost certainly legitimate (see PR discussion) — false
# positives at that scale are worse than leaving a handful of known bad
# cases to be listed here individually as they're confirmed.
#
# 20260616H1 (Kermit/N42, Arthur 2026): confirmed by operator review — the
# flight's closest approach to Arthur's track was ~377km, well inside
# reconcile_mismatched_storm_names' 500km tolerance (a real G-IV standoff
# distance, but Kermit is a penetrating P-3, not a G-IV surveillance
# aircraft), and the mission's own PDF/NetCDF metadata was miscoded with
# Arthur's storm_id despite the aircraft never actually flying the storm.
# The companion mission (20260617I1, Miss Piggy/N43) is the real Arthur
# flight and is untouched.
MANUAL_STORM_NAME_CORRECTIONS: Dict[str, str] = {
    "20260616H1": TRAINING_BUCKET_NAME,
}


def apply_manual_storm_name_corrections(conn: sqlite3.Connection) -> int:
    """One-time-idempotent fixup: applies MANUAL_STORM_NAME_CORRECTIONS.
    Clears storm_id along with storm_name so reconcile_junk_storm_buckets'
    tier-1 storm_id trust doesn't just reassign the same wrong storm right
    back on the next ingest. Runs before every other reconciliation pass so
    a manually-corrected mission is never re-derived from the same bad
    source data it was corrected because of."""
    fixed = 0
    for mission_id, storm_name in MANUAL_STORM_NAME_CORRECTIONS.items():
        cur = conn.execute(
            "UPDATE missions SET storm_name = ?, storm_id = NULL "
            "WHERE mission_id = ? AND storm_name != ?",
            (storm_name, mission_id, storm_name),
        )
        fixed += cur.rowcount
    if fixed:
        conn.commit()
    return fixed


SCHEMA = """
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
"""


def get_connection() -> sqlite3.Connection:
    conn = sqlite3.connect(str(RECON_MET_DB_PATH))
    conn.row_factory = sqlite3.Row
    conn.execute("PRAGMA foreign_keys = ON")
    conn.executescript(SCHEMA)
    return conn


# ── HTTP / directory crawling ────────────────────────────────────────────
class _HrefParser(HTMLParser):
    def __init__(self):
        super().__init__()
        self.hrefs: list[str] = []

    def handle_starttag(self, tag, attrs):
        if tag == "a":
            for k, v in attrs:
                if k == "href" and v:
                    self.hrefs.append(v)


def fetch_bytes(client: httpx.Client, url: str, timeout: float = HTTP_TIMEOUT) -> Optional[bytes]:
    try:
        resp = client.get(url, timeout=timeout)
        resp.raise_for_status()
        return resp.content
    except Exception as exc:  # noqa: BLE001 - crawling a third-party archive, keep going on any failure
        log.warning("fetch failed %s: %s", url, exc)
        return None


def list_hrefs(client: httpx.Client, url: str) -> list[str]:
    data = fetch_bytes(client, url)
    if not data:
        return []
    parser = _HrefParser()
    parser.feed(data.decode("utf-8", errors="replace"))
    return [h for h in parser.hrefs if not h.startswith("?") and h != "/"]


# ── Storm name extraction ────────────────────────────────────────────────
def extract_from_pdf(pdf_bytes: bytes) -> Tuple[Optional[str], Optional[str]]:
    """Return (storm_name, storm_id) from mission PDF bytes."""
    try:
        reader = pypdf.PdfReader(io.BytesIO(pdf_bytes))
        full_text = ""
        for page in reader.pages[:6]:
            full_text += (page.extract_text() or "") + "\n"

        storm_name = None
        m = re.search(r'Storm[:\s]+([A-Z]{2,})\s+Flight\s+ID', full_text, re.I)
        if m:
            storm_name = m.group(1).title()

        if not storm_name:
            m = re.search(
                r'NOAA\s*\d+\s+\d{3,4}[A-Z]\s+([A-Z]{2,})(?:\s|$)',
                re.sub(r'\(i\.e\.,[^)]*\)', '', full_text),
            )
            if m:
                storm_name = m.group(1).title()

        if not storm_name:
            m = re.search(r'Mission\s*ID[:\s]+\d{3,4}[A-Z]\b.{0,60}?([A-Z]{3,})', full_text)
            if m:
                candidate = m.group(1)
                if candidate not in ('FLIGHT', 'MISSION', 'LANDING', 'TAKEOFF', 'REPORT'):
                    storm_name = candidate.title()

        storm_id = None
        clean = re.sub(r'\(i\.e\.,[^)]*\)', '', full_text)
        m2 = re.search(r'((?:AL|EP|CP|WP|IO|SH)\d{6})', clean)
        storm_id = m2.group(1).upper() if m2 else None

        return storm_name, storm_id
    except Exception as exc:  # noqa: BLE001 - PDF text extraction is best-effort
        log.debug("PDF parse error: %s", exc)
        return None, None


def extract_storm_from_nc_attrs(nc_path: str) -> Optional[str]:
    """Return a Title-cased storm name from NetCDF global attributes, or None."""
    try:
        with NC_LOCK:
            ds = netCDF4.Dataset(nc_path)
            try:
                for attr in ('StormName', 'storm_name', 'storm'):
                    val = getattr(ds, attr, None)
                    if val and isinstance(val, str):
                        clean = val.strip().title()
                        if len(clean) >= 2 and clean.upper() not in ('NONE', 'N/A', 'UNKNOWN', 'TEST'):
                            return clean
                title = getattr(ds, 'title', '') or ''
                m = re.search(
                    r'(?:hurricane|tropical storm|typhoon|cyclone)\s+([A-Z][a-z]+)',
                    title, re.I,
                )
                if m:
                    return m.group(1).title()
            finally:
                ds.close()
    except Exception as exc:  # noqa: BLE001 - fallback path, best-effort
        log.debug("NC attr parse error for %s: %s", nc_path, exc)
    return None


def get_aircraft_info(client: httpx.Client, mission_url: str) -> Tuple[str, str]:
    """Return (human_name, tail_num_raw) from the mission's aampsrc config file."""
    data = fetch_bytes(client, mission_url + "aampsrc")
    if not data:
        return "Unknown Aircraft", ""
    text = data.decode("utf-8", errors="replace")
    m = re.search(r'TAILNUM=(\S+)', text, re.I)
    if not m:
        return "Unknown Aircraft", ""
    raw = m.group(1).lower().strip()
    name = TAILNUM_MAP.get(raw, f"N{raw.upper()[1:]}RF" if raw.startswith('n') else raw.upper())
    return name, raw.upper()


# ── NetCDF processing ────────────────────────────────────────────────────
def _get_masked(ds: netCDF4.Dataset, *names: str):
    for name in names:
        if name in ds.variables:
            return ds.variables[name][:]
    return None


def process_nc_file(nc_path: str, mission_id: str) -> Optional[Dict]:
    """Opens a MET NetCDF file and extracts decimated observations. Returns
    a dict with start_unix/end_unix/lat/lon bounds/obs_count/observations,
    or None on failure (unreadable file, missing variables, too few points)."""
    with NC_LOCK:
        try:
            ds = netCDF4.Dataset(nc_path)
        except Exception as exc:  # noqa: BLE001
            log.error("Cannot open %s: %s", nc_path, exc)
            return None

        try:
            start_unix = int(getattr(ds, "StartTime", 0))
            time_raw = _get_masked(ds, "Time")
            if time_raw is None or start_unix == 0:
                log.warning("%s: missing Time variable or StartTime attribute", mission_id)
                return None

            lat = _get_masked(ds, "LATref", "LatGPS.2", "LatGPS.3", "LatGPS.1")
            lon = _get_masked(ds, "LONref", "LonGPS.2", "LonGPS.3", "LonGPS.1")
            if lat is None or lon is None:
                log.warning("%s: no lat/lon variables found", mission_id)
                return None

            ws_kt = _get_masked(ds, "WSkt.d")
            wd = _get_masked(ds, "WD.d")

            sfmr_ms = _get_masked(ds, "NSfmrWS.1", "SFMRWSref", "SfmrWS.1", "ASfmrWS.1")
            sfmr_kt = sfmr_ms * 1.94384 if sfmr_ms is not None else None

            alt_m = _get_masked(ds, "ALTref", "AltGPS.2", "AltGPS.3", "AltGPS.1", "AltBCADDU.1")
        finally:
            ds.close()

    valid_mask = ~(
        np.ma.getmaskarray(time_raw) | np.ma.getmaskarray(lat) | np.ma.getmaskarray(lon)
    )
    valid_idx = np.where(valid_mask)[0]
    if len(valid_idx) < 10:
        log.warning("%s: fewer than 10 valid points, skipping", mission_id)
        return None

    sampled_idx = valid_idx[::DECIMATION]
    observations = []
    lats, lons = [], []

    for i in sampled_idx:
        t_raw, la, lo = time_raw[i], lat[i], lon[i]
        if np.ma.is_masked(t_raw) or np.ma.is_masked(la) or np.ma.is_masked(lo):
            continue

        t_unix = start_unix + int(t_raw)
        la_f, lo_f = float(la), float(lo)

        ws = float(ws_kt[i]) if ws_kt is not None and not np.ma.is_masked(ws_kt[i]) else None
        wdv = float(wd[i]) if wd is not None and not np.ma.is_masked(wd[i]) else None
        sf = float(sfmr_kt[i]) if sfmr_kt is not None and not np.ma.is_masked(sfmr_kt[i]) else None
        al = float(alt_m[i]) if alt_m is not None and not np.ma.is_masked(alt_m[i]) else None

        if not (-90 <= la_f <= 90 and -180 <= lo_f <= 180):
            continue
        # (0, 0) "null island" is a GPS/INS-dropout sentinel some flights'
        # instruments fall back to instead of properly masking the reading
        # (seen trailing the recording on some missions) — within the valid
        # range check above but not a real position for a hurricane/recon
        # flight, and left in, it corrupts both the stored lat/lon bounds
        # and any distance-based matching against it (see
        # reconcile_junk_storm_buckets in this module).
        if abs(la_f) < 0.01 and abs(lo_f) < 0.01:
            continue
        if ws is not None and (ws < 0 or ws > 300):
            ws = None
        if wdv is not None and not (0 <= wdv <= 360):
            wdv = None

        observations.append((t_unix, la_f, lo_f, ws, wdv, sf, al))
        lats.append(la_f)
        lons.append(lo_f)

    if len(observations) < 5:
        log.warning("%s: not enough valid sampled obs", mission_id)
        return None

    unix_times = [o[0] for o in observations]
    return {
        "start_unix": min(unix_times),
        "end_unix": max(unix_times),
        "lat_min": float(min(lats)),
        "lat_max": float(max(lats)),
        "lon_min": float(min(lons)),
        "lon_max": float(max(lons)),
        "obs_count": len(observations),
        "observations": observations,
    }


# ── Mission discovery ────────────────────────────────────────────────────
def get_mission_list(client: httpx.Client, year: int) -> list[str]:
    """Return mission folder names (e.g. '20241009H1') for a year."""
    url = f"{BASE_URL}/{year}/MET/"
    hrefs = list_hrefs(client, url)
    missions = []
    for h in hrefs:
        h = h.rstrip("/")
        if re.match(r"^\d{8}[A-Z]\d+$", h):
            missions.append(h)
    return missions


def get_best_nc_file(client: httpx.Client, mission_id: str, mission_url: str) -> Tuple[Optional[str], Optional[str]]:
    """Finds the highest-QC NetCDF file in a mission folder
    ({mission_id}_{LETTER}.nc, A < B < C...). Returns (filename, qc_letter)."""
    hrefs = list_hrefs(client, mission_url)
    candidates = {}
    pattern = re.compile(rf"^{re.escape(mission_id)}_([A-Z])\.nc$", re.I)
    for h in hrefs:
        m = pattern.match(h.split("/")[-1])
        if m:
            candidates[m.group(1).upper()] = h.split("/")[-1]
    if not candidates:
        return None, None
    best_letter = max(candidates.keys())
    return candidates[best_letter], best_letter


# ── Per-mission harvest ──────────────────────────────────────────────────
def harvest_mission(client: httpx.Client, conn: sqlite3.Connection, year: int, mission_id: str, force: bool = False) -> bool:
    """Downloads, processes, and stores one mission. Returns whether it was
    (re)ingested. Skips if the on-disk NC version already matches, unless
    force=True."""
    mission_url = f"{BASE_URL}/{year}/MET/{mission_id}/"

    nc_filename, nc_version = get_best_nc_file(client, mission_id, mission_url)
    if nc_filename is None:
        log.debug("%s: no QC NetCDF found, skipping", mission_id)
        return False

    row = conn.execute("SELECT nc_version FROM missions WHERE mission_id = ?", (mission_id,)).fetchone()
    if row and row["nc_version"] == nc_version and not force:
        log.debug("%s: already have version %s, skipping", mission_id, nc_version)
        return False

    log.info("%s: harvesting (NC version=%s)", mission_id, nc_version)

    storm_name = None
    storm_id = None
    all_hrefs = list_hrefs(client, mission_url)
    pdf_file = next((h for h in all_hrefs if h.lower().endswith(".pdf")), None)
    if pdf_file:
        pdf_bytes = fetch_bytes(client, mission_url + pdf_file.split("/")[-1])
        if pdf_bytes:
            storm_name, storm_id = extract_from_pdf(pdf_bytes)

    aircraft, tail_num = get_aircraft_info(client, mission_url)

    nc_url = mission_url + nc_filename
    nc_data = fetch_bytes(client, nc_url, timeout=NC_TIMEOUT)
    if nc_data is None:
        log.warning("%s: download failed", mission_id)
        return False

    with tempfile.NamedTemporaryFile(suffix=".nc", delete=False) as tmp:
        tmp.write(nc_data)
        tmp_path = tmp.name

    try:
        if not storm_name:
            storm_name = extract_storm_from_nc_attrs(tmp_path)
        if not storm_name:
            storm_name = TRAINING_BUCKET_NAME

        result = process_nc_file(tmp_path, mission_id)
        if result is None:
            return False

        now = int(time.time())
        flight_date = datetime.datetime.fromtimestamp(result["start_unix"], tz=datetime.timezone.utc).strftime("%Y-%m-%d")

        conn.execute("BEGIN")
        conn.execute("""
            INSERT INTO missions
              (mission_id, year, storm_name, storm_id, aircraft, tail_num,
               flight_date, start_unix, end_unix, nc_version, source_url,
               lat_min, lat_max, lon_min, lon_max, obs_count, fetched_at)
            VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)
            ON CONFLICT(mission_id) DO UPDATE SET
              storm_name=excluded.storm_name, storm_id=excluded.storm_id,
              aircraft=excluded.aircraft, tail_num=excluded.tail_num,
              flight_date=excluded.flight_date, start_unix=excluded.start_unix,
              end_unix=excluded.end_unix, nc_version=excluded.nc_version,
              source_url=excluded.source_url,
              lat_min=excluded.lat_min, lat_max=excluded.lat_max,
              lon_min=excluded.lon_min, lon_max=excluded.lon_max,
              obs_count=excluded.obs_count, fetched_at=excluded.fetched_at
        """, (
            mission_id, year, storm_name, storm_id, aircraft, tail_num,
            flight_date, result["start_unix"], result["end_unix"], nc_version, nc_url,
            result["lat_min"], result["lat_max"], result["lon_min"], result["lon_max"],
            result["obs_count"], now,
        ))
        conn.execute("DELETE FROM observations WHERE mission_id = ?", (mission_id,))
        conn.executemany(
            "INSERT INTO observations (mission_id, seq_num, unix_time, lat, lon, wind_kt, wind_dir, sfmr_kt, alt_m) "
            "VALUES (?,?,?,?,?,?,?,?,?)",
            [
                (mission_id, seq_num, t, la, lo, ws, wdv, sf, al)
                for seq_num, (t, la, lo, ws, wdv, sf, al) in enumerate(result["observations"])
            ],
        )
        conn.execute("COMMIT")
        log.info("%s: stored %d obs (version %s)", mission_id, result["obs_count"], nc_version)
        return True
    except Exception:
        conn.execute("ROLLBACK")
        raise
    finally:
        os.unlink(tmp_path)


def clean_null_island_observations(conn: sqlite3.Connection) -> int:
    """One-time-idempotent fixup: deletes any already-stored (0, 0) "null
    island" observation rows — see the ingestion-time filter added to
    process_nc_file() above, which only stops *new* harvests from storing
    these; missions harvested before that filter existed can still have
    them sitting in the database, which shows up as a bogus jump to the
    Gulf of Guinea on any client that plots a mission's track. Recomputes
    each affected mission's obs_count/lat_min/lat_max/lon_min/lon_max
    afterward so they stay consistent with the remaining real observations.
    Returns the number of observation rows removed."""
    affected = conn.execute(
        "SELECT DISTINCT mission_id FROM observations WHERE ABS(lat) < 0.01 AND ABS(lon) < 0.01"
    ).fetchall()
    if not affected:
        return 0

    removed = 0
    for row in affected:
        mid = row["mission_id"]
        cur = conn.execute(
            "DELETE FROM observations WHERE mission_id = ? AND ABS(lat) < 0.01 AND ABS(lon) < 0.01", (mid,)
        )
        removed += cur.rowcount
        stats = conn.execute(
            "SELECT COUNT(*) AS c, MIN(lat) AS lat_min, MAX(lat) AS lat_max, "
            "MIN(lon) AS lon_min, MAX(lon) AS lon_max FROM observations WHERE mission_id = ?",
            (mid,),
        ).fetchone()
        conn.execute(
            "UPDATE missions SET obs_count=?, lat_min=?, lat_max=?, lon_min=?, lon_max=? WHERE mission_id=?",
            (stats["c"], stats["lat_min"], stats["lat_max"], stats["lon_min"], stats["lon_max"], mid),
        )
    conn.commit()
    return removed


def migrate_unknown_storm_names(conn: sqlite3.Connection) -> int:
    """One-time-idempotent fixup: any mission whose storm_name is literally
    its own raw mission ID (from old harvests, before the TRAINING_BUCKET_NAME
    fallback existed) gets renamed so the archive groups them together
    instead of one bucket per flight. Safe to call after every harvest — a
    no-op once the fixup has already applied."""
    rows = conn.execute("SELECT mission_id, storm_name FROM missions").fetchall()
    to_update = [r["mission_id"] for r in rows if r["storm_name"] and re.match(r'^\d{8}[A-Z]\d+$', r["storm_name"])]
    if to_update:
        conn.executemany(
            "UPDATE missions SET storm_name = ? WHERE mission_id = ?",
            [(TRAINING_BUCKET_NAME, mid) for mid in to_update],
        )
        conn.commit()
    return len(to_update)


def rename_legacy_training_bucket(conn: sqlite3.Connection) -> int:
    """One-time-idempotent fixup: renames missions still carrying a
    pre-rename training-bucket name (see LEGACY_TRAINING_BUCKET_NAMES)
    forward to TRAINING_BUCKET_NAME, so archives harvested before the rename
    don't end up split across two differently-named buckets. Safe to call
    after every harvest — a no-op once applied."""
    renamed = 0
    for legacy_name in LEGACY_TRAINING_BUCKET_NAMES:
        cur = conn.execute(
            "UPDATE missions SET storm_name = ? WHERE storm_name = ?",
            (TRAINING_BUCKET_NAME, legacy_name),
        )
        renamed += cur.rowcount
    if renamed:
        conn.commit()
    return renamed


def reconcile_storm_ids(conn: sqlite3.Connection) -> int:
    """One-time-idempotent fixup: PDF/NetCDF-attribute name extraction
    sometimes finds a storm_id but not a storm_name for a flight, even
    though another flight with the *same* storm_id got named fine (e.g.
    that mission's PDF had a clean "Storm: DORIAN" line and a sibling
    flight's didn't). When a storm_id has exactly one distinct real name
    attached anywhere, every TRAINING_BUCKET_NAME mission sharing that
    storm_id gets relabeled to match — the named flight's data effectively
    absorbs the unnamed one's.

    Deliberately does NOT touch storm_ids with more than one distinct real
    name attached (e.g. a botched extraction pulling "Invest" or "Survey"
    as if it were a name, or a storm_id genuinely misread off a stray PDF
    example) — there's no safe automatic pick there, so those are left for
    manual review rather than guessed at."""
    groups = conn.execute("""
        SELECT storm_id,
               COUNT(DISTINCT CASE WHEN storm_name != ? THEN storm_name END) AS real_name_count,
               MAX(CASE WHEN storm_name != ? THEN storm_name END) AS real_name
        FROM missions
        WHERE storm_id IS NOT NULL AND storm_id != ''
        GROUP BY storm_id
        HAVING real_name_count = 1 AND SUM(CASE WHEN storm_name = ? THEN 1 ELSE 0 END) > 0
    """, (TRAINING_BUCKET_NAME, TRAINING_BUCKET_NAME, TRAINING_BUCKET_NAME)).fetchall()

    fixed = 0
    for g in groups:
        cur = conn.execute(
            "UPDATE missions SET storm_name = ? WHERE storm_id = ? AND storm_name = ?",
            (g["real_name"], g["storm_id"], TRAINING_BUCKET_NAME),
        )
        fixed += cur.rowcount
    if fixed:
        conn.commit()
    return fixed


# ── Junk storm-name bucket reconciliation ───────────────────────────────
# NOAA's directory metadata mis-files some flights under a handful of
# generic bucket names instead of the real storm they flew — confirmed in
# the wild for 2024: "Tdr" and "Surv"/"Survey" (surveillance-flight jargon
# leaking into the storm-name field) carrying Helene's/Debby's real
# storm_id, "Cyclone" (a generic label, seen on G-IV synoptic-surveillance
# flights) carrying Hone's real storm_id, and both "Invest" and a big chunk
# of the TRAINING_BUCKET_NAME bucket carrying a stale storm_id (AL072012 —
# a *real* 2012 storm, itself also coincidentally named Helene, most likely
# a leaked PDF-template example the "(i.e., ...)" strip in extract_from_pdf()
# didn't catch) that has nothing to do with the actual 2024 flight.
JUNK_STORM_NAMES = {"CYCLONE", "TDR", "SURV", "SURVEY", "RECON", "INVEST", TRAINING_BUCKET_NAME.upper()}

# Recon flights (especially G-IV synoptic-surveillance legs) can operate a
# few hundred km from a storm's center; ATCF/HURDAT2 track points land every
# 6 hours. Both bounds are deliberately generous — a false negative just
# leaves a mission in its junk bucket for manual review, same as the
# existing reconcile_storm_ids() fixup; a false positive would misfile it
# under the wrong storm, so neither bound should be loosened further
# without also tightening the other.
MAX_STORM_MATCH_DISTANCE_KM = 500
MAX_STORM_MATCH_TIME_HOURS = 30


def _haversine_km(lat1: float, lon1: float, lat2: float, lon2: float) -> float:
    r = 6371.0
    p1, p2 = math.radians(lat1), math.radians(lat2)
    dphi = math.radians(lat2 - lat1)
    dlambda = math.radians(lon2 - lon1)
    a = math.sin(dphi / 2) ** 2 + math.cos(p1) * math.cos(p2) * math.sin(dlambda / 2) ** 2
    return 2 * r * math.asin(math.sqrt(a))


def _mission_track_points(conn: sqlite3.Connection, mission_id: str) -> List[Tuple[float, float]]:
    """All of a mission's decimated (lat, lon) observations, excluding (0, 0)
    "null island" readings — a GPS/INS-dropout artifact seen trailing some
    flights' recordings (e.g. the last ~30 observations of 20221030I1), not
    a real position; no Atlantic/Pacific hurricane recon flight is ever
    actually at the equator/prime meridian."""
    rows = conn.execute(
        "SELECT lat, lon FROM observations WHERE mission_id = ? AND NOT (ABS(lat) < 0.01 AND ABS(lon) < 0.01)",
        (mission_id,),
    ).fetchall()
    return [(r["lat"], r["lon"]) for r in rows]


def _mission_min_distance_km(points: List[Tuple[float, float]], lat: float, lon: float) -> Optional[float]:
    """Closest approach (km) of any of the mission's own flown positions to
    (lat, lon) — used instead of the mission's position at one exact
    instant. A single-moment snapshot is fragile for a short repositioning/
    transit leg (e.g. 20250928N2: a 42-minute hop from 300km off Imelda back
    to home base, whose position 20+ minutes after landing — the storm
    track's nearest synoptic hour — is the *far* end of that hop, not the
    close approach) or a flight with corrupted trailing readings (see
    _mission_track_points above); the flight's closest approach across its
    whole track is what actually indicates it was flying that storm."""
    if not points:
        return None
    return min(_haversine_km(la, lo, lat, lon) for la, lo in points)


def _storm_track_distance_km(storms_conn: sqlite3.Connection, points: List[Tuple[float, float]],
                              storm_row_id: int, mid_unix: float) -> Optional[float]:
    """Closest approach (km) of `points` (a mission's flown track) to the
    given storm's own track point nearest in time to `mid_unix`, or None if
    that storm has no track point within MAX_STORM_MATCH_TIME_HOURS — shared
    by both _find_matching_storm (searching every storm for the best match)
    and reconcile_mismatched_storm_names (checking one specific, already-
    assigned storm)."""
    mid_dt = datetime.datetime.fromtimestamp(mid_unix, tz=datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    point = storms_conn.execute(
        "SELECT datetime_utc, lat, lon FROM track_points WHERE storm_id = ? "
        "ORDER BY ABS(julianday(datetime_utc) - julianday(?)) LIMIT 1",
        (storm_row_id, mid_dt),
    ).fetchone()
    if point is None:
        return None
    point_dt = datetime.datetime.strptime(point["datetime_utc"], "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=datetime.timezone.utc)
    hours_off = abs(point_dt.timestamp() - mid_unix) / 3600
    if hours_off > MAX_STORM_MATCH_TIME_HOURS:
        return None
    return _mission_min_distance_km(points, point["lat"], point["lon"])


def _find_matching_storm(storms_conn: sqlite3.Connection, recon_conn: sqlite3.Connection,
                          mission_id: str, year: int, mid_unix: float) -> Optional[sqlite3.Row]:
    """Among storms active around `year` (+/- 1, in case a mission lands
    right at a year boundary relative to the storm's own year field), finds
    the one whose track passed nearest this mission's flown path around the
    same time — the real signal that a recon flight was flying that storm,
    independent of whatever bucket name NOAA's directory metadata assigned
    it. Returns the storms row, or None if nothing is within
    MAX_STORM_MATCH_DISTANCE_KM/MAX_STORM_MATCH_TIME_HOURS."""
    points = _mission_track_points(recon_conn, mission_id)
    if not points:
        return None

    candidates = storms_conn.execute(
        "SELECT id, atcf_id, basin, year, name FROM storms WHERE year IN (?, ?, ?)",
        (year - 1, year, year + 1),
    ).fetchall()

    best_storm, best_distance = None, None
    for s in candidates:
        dist_km = _storm_track_distance_km(storms_conn, points, s["id"], mid_unix)
        if dist_km is None or dist_km > MAX_STORM_MATCH_DISTANCE_KM:
            continue
        if best_distance is None or dist_km < best_distance:
            best_storm, best_distance = s, dist_km
    return best_storm


def reconcile_junk_storm_buckets(conn: sqlite3.Connection) -> int:
    """One-time-idempotent fixup: missions filed under a generic junk bucket
    (see JUNK_STORM_NAMES) get reassigned to their real storm, in two tiers:

    1. Trust the mission's own storm_id if it's internally consistent — its
       embedded year (the last 4 digits of an "AL092024"-style ATCF id)
       matches this mission's actual year — and look it up directly in the
       storms archive. Resolves "Tdr"/"Surv"/"Survey"/"Cyclone" cases, which
       carry a correct storm_id despite the wrong name.
    2. Otherwise (no storm_id, or a stale one from a different year — e.g.
       "Invest"/TRAINING_BUCKET_NAME missions carrying a leaked 2012
       example id on a 2024 flight), fall back to matching the mission's own
       flown position against every storm's track around that time (see
       _find_matching_storm). A flight with no real storm nearby (e.g. a
       January Pacific atmospheric-river mission with no tropical system in
       play at all) correctly finds no match and stays in its bucket rather
       than being forced onto an unrelated storm.

    Safe to call after every harvest — a no-op for missions already
    correctly named, and for genuine non-tropical training/survey flights
    that will never match tier 1 or 2.

    Skips every mission_id in MANUAL_STORM_NAME_CORRECTIONS: tier 2 matches
    purely by position, the same signal that produced the wrong name in
    the first place for a manually-corrected mission (e.g. 20260616H1 —
    the flight really was within tier 2's own distance/time tolerance of
    the storm it was wrongly coded for), so leaving those in scope would
    silently re-derive the bad name right back on the very next ingest."""
    from app.services import storms as storms_svc  # local import: avoids a module-load-order dependency

    name_placeholders = ",".join("?" * len(JUNK_STORM_NAMES))
    query = (
        f"SELECT mission_id, year, storm_id, start_unix, end_unix FROM missions "
        f"WHERE UPPER(storm_name) IN ({name_placeholders})"
    )
    params = list(JUNK_STORM_NAMES)
    if MANUAL_STORM_NAME_CORRECTIONS:
        id_placeholders = ",".join("?" * len(MANUAL_STORM_NAME_CORRECTIONS))
        query += f" AND mission_id NOT IN ({id_placeholders})"
        params += list(MANUAL_STORM_NAME_CORRECTIONS.keys())
    junk_rows = conn.execute(query, params).fetchall()
    if not junk_rows:
        return 0

    storms_conn = storms_svc.get_connection()
    try:
        fixed = 0
        for row in junk_rows:
            match = None

            sid = (row["storm_id"] or "").strip().upper()
            m = re.match(r"^[A-Z]{2}\d{2}(\d{4})$", sid)
            if m and int(m.group(1)) == row["year"]:
                match = storms_conn.execute(
                    "SELECT id, atcf_id, basin, year, name FROM storms WHERE atcf_id = ?", (sid,)
                ).fetchone()

            if match is None:
                mid_unix = (row["start_unix"] + row["end_unix"]) / 2
                match = _find_matching_storm(storms_conn, conn, row["mission_id"], row["year"], mid_unix)

            if match is None:
                continue
            conn.execute(
                "UPDATE missions SET storm_name = ?, storm_id = ? WHERE mission_id = ?",
                (match["name"].title(), match["atcf_id"], row["mission_id"]),
            )
            fixed += 1
        if fixed:
            conn.commit()
        return fixed
    finally:
        storms_conn.close()


# ── Named-storm mismatch reconciliation ─────────────────────────────────
# extract_from_pdf()/extract_storm_from_nc_attrs() take whatever storm name
# NOAA's own mission documents say at face value — but a document can name
# a storm that has nothing to do with where the aircraft actually flew
# (e.g. a Houston AIRMAPS instrument-calibration exercise whose paperwork
# happens to reference a storm by name without the flight ever going near
# it). Trusting the document name alone mislabels flights like that as real
# storm missions. This section demotes any such mismatch back into
# TRAINING_BUCKET_NAME so the position-based tier of
# reconcile_junk_storm_buckets() above gets a chance to find (or rule out)
# the flight's real storm from where it actually flew — the same
# "chronological + lat/lon" fallback that bucket already uses for flights
# with no document name at all.
def reconcile_mismatched_storm_names(conn: sqlite3.Connection) -> int:
    """One-time-idempotent fixup: for every mission NOT already in a junk
    bucket (see JUNK_STORM_NAMES) — i.e. one carrying a real-looking,
    document-derived storm name — confirms the mission's own flown track
    actually came within MAX_STORM_MATCH_DISTANCE_KM of that *named* storm's
    track within MAX_STORM_MATCH_TIME_HOURS of the mission's midpoint (the
    same signal reconcile_junk_storm_buckets uses to find a storm from
    position alone, just checked against the one specific storm this
    mission already claims rather than searched for). A mission whose name
    doesn't correspond to any storm active within a year of its own, or
    whose track never came close to the storm it's named for, gets
    demoted to TRAINING_BUCKET_NAME with its storm_id cleared — clearing
    storm_id matters because reconcile_junk_storm_buckets' tier 1 trusts
    any internally-consistent storm_id on sight, and would otherwise just
    reassign the same wrong storm right back on the very next ingest.

    Skips missions with no stored observations to check a track against —
    left alone rather than guessed at, same as reconcile_junk_storm_buckets'
    own tier 2. Safe to call after every harvest — idempotent (a no-op
    *outcome* for missions already confirmed or already sitting in a junk
    bucket this function doesn't touch) but not cheap: unlike this file's
    other reconcile_* fixups, which only ever scan the small JUNK_STORM_NAMES
    subset, this one re-walks every named mission's full observation track
    on every call regardless of outcome (confirmed live: ~90s over ~650
    named missions) — a per-ingest cost that grows with the archive, not
    a one-time convergence cost. Acceptable for a nightly cron cadence;
    revisit (e.g. a "verified" flag/timestamp column so already-confirmed
    missions are skipped) if the archive grows enough for that to change."""
    from app.services import storms as storms_svc  # local import: avoids a module-load-order dependency

    placeholders = ",".join("?" * len(JUNK_STORM_NAMES))
    named_rows = conn.execute(
        f"SELECT mission_id, year, storm_name, start_unix, end_unix FROM missions "
        f"WHERE UPPER(storm_name) NOT IN ({placeholders})",
        tuple(JUNK_STORM_NAMES),
    ).fetchall()
    if not named_rows:
        return 0

    storms_conn = storms_svc.get_connection()
    try:
        demoted = 0
        for row in named_rows:
            points = _mission_track_points(conn, row["mission_id"])
            if not points:
                continue

            candidates = storms_conn.execute(
                "SELECT id FROM storms WHERE name = ? COLLATE NOCASE AND year IN (?, ?, ?)",
                (row["storm_name"], row["year"] - 1, row["year"], row["year"] + 1),
            ).fetchall()

            mid_unix = (row["start_unix"] + row["end_unix"]) / 2
            confirmed = False
            for c in candidates:
                dist_km = _storm_track_distance_km(storms_conn, points, c["id"], mid_unix)
                if dist_km is not None and dist_km <= MAX_STORM_MATCH_DISTANCE_KM:
                    confirmed = True
                    break

            if not confirmed:
                conn.execute(
                    "UPDATE missions SET storm_name = ?, storm_id = NULL WHERE mission_id = ?",
                    (TRAINING_BUCKET_NAME, row["mission_id"]),
                )
                demoted += 1
        if demoted:
            conn.commit()
        return demoted
    finally:
        storms_conn.close()


# ── Query helpers used by app/routers/recon.py ──────────────────────────
def list_years(conn: sqlite3.Connection) -> list[int]:
    return [r["year"] for r in conn.execute("SELECT DISTINCT year FROM missions ORDER BY year").fetchall()]


def list_storms_for_year(conn: sqlite3.Connection, year: int) -> list[sqlite3.Row]:
    return conn.execute(
        "SELECT storm_name, COUNT(*) AS mission_count, MAX(storm_id) AS storm_id "
        "FROM missions WHERE year = ? GROUP BY storm_name ORDER BY storm_name",
        (year,),
    ).fetchall()


def list_missions_for_storm(conn: sqlite3.Connection, year: int, storm_name: str) -> list[sqlite3.Row]:
    return conn.execute(
        "SELECT mission_id, aircraft, tail_num, flight_date, start_unix, end_unix, "
        "obs_count, source_url FROM missions "
        "WHERE year = ? AND storm_name = ? COLLATE NOCASE ORDER BY start_unix",
        (year, storm_name),
    ).fetchall()


def get_mission(conn: sqlite3.Connection, mission_id: str) -> Optional[sqlite3.Row]:
    return conn.execute("SELECT * FROM missions WHERE mission_id = ?", (mission_id,)).fetchone()


def get_observations(conn: sqlite3.Connection, mission_id: str) -> list[sqlite3.Row]:
    return conn.execute(
        "SELECT unix_time, lat, lon, wind_kt, wind_dir, sfmr_kt, alt_m "
        "FROM observations WHERE mission_id = ? ORDER BY seq_num",
        (mission_id,),
    ).fetchall()


# ── Full ingest orchestration — used by both scripts/ingest_recon_met.py
# (CLI / nightly systemd timer) and the admin console's "force update"
# button (app/routers/admin.py), so there's exactly one place this pipeline
# is defined ─────────────────────────────────────────────────────────────
def run_ingest(years: Optional[list[int]] = None, force: bool = False) -> dict:
    """Crawls the given years (default: current + previous, matching the
    nightly timer) and upserts every mission found. Safe to call
    repeatedly — each mission is skipped unless its NC QC version changed,
    unless force=True. Returns a JSON-serializable summary."""
    if years is None:
        current_year = datetime.datetime.now(datetime.timezone.utc).year
        years = [current_year - 1, current_year]

    conn = get_connection()
    summary: dict = {"years": {}, "ingested": 0, "skipped": 0, "errors": 0}
    with httpx.Client(timeout=HTTP_TIMEOUT, follow_redirects=True) as client:
        for year in years:
            mission_ids = get_mission_list(client, year)
            year_ingested = 0
            for mission_id in mission_ids:
                try:
                    if harvest_mission(client, conn, year, mission_id, force=force):
                        year_ingested += 1
                        summary["ingested"] += 1
                    else:
                        summary["skipped"] += 1
                except Exception as exc:  # noqa: BLE001 - keep crawling other missions
                    log.error("%s: error during harvest: %s", mission_id, exc)
                    summary["errors"] += 1
            summary["years"][str(year)] = year_ingested

    cleaned = clean_null_island_observations(conn)
    if cleaned:
        summary["null_island_obs_removed"] = cleaned
    corrected = apply_manual_storm_name_corrections(conn)
    if corrected:
        summary["manual_corrections_applied"] = corrected
    fixed = migrate_unknown_storm_names(conn)
    if fixed:
        summary["legacy_names_fixed"] = fixed
    renamed = rename_legacy_training_bucket(conn)
    if renamed:
        summary["legacy_bucket_renamed"] = renamed
    reconciled = reconcile_storm_ids(conn)
    if reconciled:
        summary["storm_ids_reconciled"] = reconciled
    mismatched = reconcile_mismatched_storm_names(conn)
    if mismatched:
        summary["mismatched_names_demoted"] = mismatched
    junk_fixed = reconcile_junk_storm_buckets(conn)
    if junk_fixed:
        summary["junk_buckets_reconciled"] = junk_fixed
    summary["total_missions"] = conn.execute("SELECT COUNT(*) FROM missions").fetchone()[0]
    conn.close()
    return summary
