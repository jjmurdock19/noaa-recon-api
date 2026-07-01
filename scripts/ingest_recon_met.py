#!/usr/bin/env python3
"""Crawl NOAA's raw recon aircraft archive and (re)populate
data/recon_met.sqlite. Thin CLI wrapper — the actual crawl/parse pipeline
lives in app.services.recon_met.run_ingest() so it's one code path shared
with the admin console's "force update" button (app/routers/admin.py).

Usage:
  .venv/bin/python3 scripts/ingest_recon_met.py               # current + previous year (nightly default)
  .venv/bin/python3 scripts/ingest_recon_met.py --full         # crawl every year from scratch (2011-present)
  .venv/bin/python3 scripts/ingest_recon_met.py --year 2024    # one year only
  .venv/bin/python3 scripts/ingest_recon_met.py --recent 3      # most recent N years

Idempotent: each mission is skipped if the already-stored NC QC version
matches what's on the server, so a nightly re-run only does real work for
missions that are new or got a QC upgrade. Installed as a nightly systemd
timer — see deploy/recon-met-update.timer (install instructions in
deploy/recon-met-update.service's header comment).
"""
import argparse
import datetime
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from app.services import recon_met  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="NOAA recon MET archive harvester")
    parser.add_argument("--full", action="store_true", help="Force re-harvest all years from scratch")
    parser.add_argument("--year", type=int, help="Process a single year only")
    parser.add_argument("--recent", type=int, default=0, help="Process only the most recent N years")
    args = parser.parse_args()

    current_year = datetime.datetime.now(datetime.timezone.utc).year
    if args.year:
        years = [args.year]
    elif args.recent:
        years = list(range(current_year - args.recent + 1, current_year + 1))
    elif args.full:
        years = list(range(recon_met.FIRST_YEAR, current_year + 1))
    else:
        years = None  # run_ingest() defaults to current + previous year

    summary = recon_met.run_ingest(years=years, force=args.full)
    print(f"By year: {summary['years']}")
    print(f"Done. {summary['ingested']} ingested, {summary['skipped']} unchanged, {summary['errors']} errors, "
          f"{summary['total_missions']} missions total in {recon_met.RECON_MET_DB_PATH}")


if __name__ == "__main__":
    main()
