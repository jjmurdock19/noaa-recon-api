#!/usr/bin/env python3
"""One-time local migration: copies the hurricanes site's already-harvested
met_archive.sqlite (1,668 missions / ~8.4M observations as of this port)
straight into this project's data/recon_met.sqlite, instead of re-crawling
years of identical data from NOAA over the network.

Not part of the "deploy elsewhere" story — a fresh deployment with no
pre-existing archive should just run scripts/ingest_recon_met.py --full,
which crawls from scratch. This script only exists because *this specific
host* already had the processed result sitting right next to it.

Usage:
  .venv/bin/python3 scripts/import_existing_met_archive.py [path/to/met_archive.sqlite]
"""
import sqlite3
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from app.services import recon_met  # noqa: E402

DEFAULT_SOURCE = Path("/var/www/html/hurricanes/met_archive.sqlite")


def main():
    source_path = Path(sys.argv[1]) if len(sys.argv) > 1 else DEFAULT_SOURCE
    if not source_path.exists():
        print(f"Source database not found: {source_path}")
        sys.exit(1)

    src = sqlite3.connect(str(source_path))
    src.row_factory = sqlite3.Row
    dst = recon_met.get_connection()

    missions = src.execute("SELECT * FROM missions").fetchall()
    print(f"Importing {len(missions)} missions from {source_path} ...")

    dst.execute("BEGIN")
    for m in missions:
        source_url = f"{recon_met.BASE_URL}/{m['year']}/MET/{m['mission_id']}/{m['mission_id']}_{m['nc_version']}.nc"
        dst.execute("""
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
            m["mission_id"], m["year"], m["storm_name"], m["storm_id"], m["aircraft"], m["tail_num"],
            m["flight_date"], m["start_unix"], m["end_unix"], m["nc_version"], source_url,
            m["lat_min"], m["lat_max"], m["lon_min"], m["lon_max"], m["obs_count"], m["fetched_at"],
        ))

        obs_rows = src.execute(
            "SELECT seq_num, unix_time, lat, lon, wind_kt, wind_dir, sfmr_kt, alt_m "
            "FROM observations WHERE mission_id = ? ORDER BY seq_num",
            (m["mission_id"],),
        ).fetchall()
        dst.execute("DELETE FROM observations WHERE mission_id = ?", (m["mission_id"],))
        dst.executemany(
            "INSERT INTO observations (mission_id, seq_num, unix_time, lat, lon, wind_kt, wind_dir, sfmr_kt, alt_m) "
            "VALUES (?,?,?,?,?,?,?,?,?)",
            [(m["mission_id"], r["seq_num"], r["unix_time"], r["lat"], r["lon"], r["wind_kt"], r["wind_dir"], r["sfmr_kt"], r["alt_m"]) for r in obs_rows],
        )
    dst.execute("COMMIT")

    fixed = recon_met.migrate_unknown_storm_names(dst)
    reconciled = recon_met.reconcile_storm_ids(dst)
    total_missions = dst.execute("SELECT COUNT(*) FROM missions").fetchone()[0]
    total_obs = dst.execute("SELECT COUNT(*) FROM observations").fetchone()[0]
    src.close()
    dst.close()
    print(f"Done. {total_missions} missions, {total_obs} observations in {recon_met.RECON_MET_DB_PATH}"
          f"{f' ({fixed} legacy names fixed)' if fixed else ''}"
          f"{f' ({reconciled} unnamed missions reconciled to a sibling storm_id name)' if reconciled else ''}")


if __name__ == "__main__":
    main()
