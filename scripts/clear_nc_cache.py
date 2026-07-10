#!/usr/bin/env python3
"""Delete cached raw GOES netCDF files older than 1 day from cache/goes_nc.

These are the raw source files ensure_downloaded() (app/services/goes.py)
fetches from S3 before rendering — safe to delete any time since they're
just re-downloaded on demand the next time they're needed. Rendered
PNG/JSON results in cache/satellite are untouched (those are cleared
manually from the admin console, not on a timer).

Run manually:
    .venv/bin/python3 scripts/clear_nc_cache.py [--max-age-hours 24] [--dry-run]

Installed as a nightly systemd timer — see deploy/goes-nc-cache-cleanup.timer
and deploy/goes-nc-cache-cleanup.service (install instructions in that
file's header comment).
"""
import argparse
import datetime
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from app.paths import CACHE_ROOT  # noqa: E402

NC_CACHE_DIR = CACHE_ROOT / "goes_nc"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--max-age-hours", type=float, default=24,
        help="delete files whose mtime is older than this many hours (default: 24)",
    )
    parser.add_argument("--dry-run", action="store_true", help="list what would be deleted without deleting")
    args = parser.parse_args()

    cutoff = datetime.datetime.now().timestamp() - args.max_age_hours * 3600
    freed = 0
    removed = 0
    if NC_CACHE_DIR.exists():
        for p in NC_CACHE_DIR.iterdir():
            if not p.is_file() or p.stat().st_mtime >= cutoff:
                continue
            size = p.stat().st_size
            if args.dry_run:
                print(f"Would delete {p.name} ({size} bytes)")
            else:
                p.unlink()
                print(f"Deleted {p.name} ({size} bytes)")
            freed += size
            removed += 1

    verb = "Would free" if args.dry_run else "Freed"
    print(f"{verb} {freed} bytes across {removed} file(s) older than {args.max_age_hours}h.")


if __name__ == "__main__":
    main()
