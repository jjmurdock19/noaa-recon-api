"""Historical storm track lookups, backed by NHC HURDAT2 data — see
app/services/storms.py for the parser/schema and scripts/ingest_storms.py
for how the database gets populated (run manually, like archive-indexer.php
does for the recon HDOB archive in the sibling hurricanes project).
"""
import datetime as dt
from typing import Optional

from fastapi import APIRouter, HTTPException, Query

from app.services import storms

router = APIRouter(prefix="/storms", tags=["storms"])


def _row_to_point(row) -> dict:
    return {
        "datetime_utc": row["datetime_utc"],
        "status": row["status"],
        "category": row["category"],
        "lat": row["lat"],
        "lon": row["lon"],
        "wind_kt": row["wind_kt"],
        "pressure_mb": row["pressure_mb"],
    }


def _resolve_one_storm(conn, year: int, name: str, basin: Optional[str]):
    matches = storms.find_storms(conn, year, name, basin)
    if not matches:
        raise HTTPException(404, f"No storm named {name!r} found in {year}.")
    if len(matches) > 1:
        options = ", ".join(f"{m['name']} ({m['basin']} — {m['atcf_id']})" for m in matches)
        raise HTTPException(
            409,
            f"Ambiguous: multiple storms named {name!r} in {year} ({options}). Pass basin=AL|EP|CP to disambiguate.",
        )
    return matches[0]


@router.get("/years")
async def list_years():
    """Every year with at least one storm in the database."""
    conn = storms.get_connection()
    try:
        return {"years": storms.list_years(conn)}
    finally:
        conn.close()


@router.get("/{year}")
async def list_storms_for_year(year: int):
    """Every storm (Atlantic + East/Central Pacific) tracked in a given year."""
    conn = storms.get_connection()
    try:
        rows = storms.list_storms_for_year(conn, year)
        if not rows:
            raise HTTPException(404, f"No storms found for year {year}.")
        return {
            "year": year,
            "storms": [{"name": r["name"], "basin": r["basin"], "atcf_id": r["atcf_id"]} for r in rows],
        }
    finally:
        conn.close()


@router.get("/{year}/{name}")
async def get_storm_track(year: int, name: str, basin: Optional[str] = Query(None, description="AL, EP, or CP — disambiguates if the name is reused across basins in the same year")):
    """Full best-track (every 6-hourly fix) for one storm."""
    conn = storms.get_connection()
    try:
        storm = _resolve_one_storm(conn, year, name, basin)
        points = storms.get_track(conn, storm["id"])
        return {
            "year": storm["year"],
            "name": storm["name"],
            "basin": storm["basin"],
            "atcf_id": storm["atcf_id"],
            "points": [_row_to_point(p) for p in points],
        }
    finally:
        conn.close()


@router.get("/{year}/{name}/nearest")
async def get_nearest_point(
    year: int,
    name: str,
    datetime: str = Query(..., description="ISO 8601 UTC datetime, e.g. 2023-09-10T12:00:00Z"),
    basin: Optional[str] = Query(None, description="AL, EP, or CP — disambiguates if the name is reused across basins in the same year"),
):
    """The single best-track fix closest in time to an arbitrary datetime —
    the "feed a year, storm name, and datetime" lookup."""
    try:
        dt.datetime.fromisoformat(datetime.replace("Z", "+00:00"))
    except ValueError as e:
        raise HTTPException(400, f"datetime must be ISO 8601: {e}") from e

    conn = storms.get_connection()
    try:
        storm = _resolve_one_storm(conn, year, name, basin)
        point = storms.find_nearest_point(conn, storm["id"], datetime)
        if point is None:
            raise HTTPException(404, "Storm has no track points on record.")
        result = _row_to_point(point)
        result.update({"year": storm["year"], "name": storm["name"], "basin": storm["basin"], "atcf_id": storm["atcf_id"]})
        return result
    finally:
        conn.close()
