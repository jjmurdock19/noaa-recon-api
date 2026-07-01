#!/usr/bin/env python3
"""(Re)populate data/storms.sqlite from NOAA/NHC storm track data.

Thin CLI wrapper — the actual HURDAT2 + ATCF ingest pipeline lives in
app.services.storms.run_ingest() so it's one code path shared with the
admin console's "force update" button (app/routers/admin.py). See that
module's docstring for the full pipeline description (three passes:
HURDAT2, ATCF archived gap-years, ATCF live current season) and
is_real_storm_name() for why only named storms are kept.

Run manually:
    .venv/bin/python3 scripts/ingest_storms.py

Installed as a nightly systemd timer — see deploy/storm-archive-update.timer
and deploy/storm-archive-update.service (install instructions in that
file's header comment).
"""
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from app.services import storms  # noqa: E402


def main():
    summary = storms.run_ingest()
    print(f"HURDAT2: {summary['hurdat2']}")
    print(f"ATCF gap/live fill: {summary['atcf']}")
    print(f"Done. {summary['total_storms']} named storms total in {storms.STORMS_DB_PATH}")


if __name__ == "__main__":
    main()
