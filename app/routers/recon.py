"""Recon MET (1-second flight-level observation) archive endpoints — see
app/services/recon_met.py for the crawler/schema and
scripts/ingest_recon_met.py for how the database gets populated.
"""
import httpx
from fastapi import APIRouter, HTTPException
from fastapi.responses import StreamingResponse

from app.services import recon_met

router = APIRouter(prefix="/recon", tags=["recon"])


def _mission_summary(row) -> dict:
    return {
        "mission_id": row["mission_id"],
        "aircraft": row["aircraft"],
        "tail_num": row["tail_num"],
        "flight_date": row["flight_date"],
        "start_unix": row["start_unix"],
        "end_unix": row["end_unix"],
        "obs_count": row["obs_count"],
        "source_url": row["source_url"],
    }


@router.get("/years")
async def list_years():
    """Every year with at least one recon mission archived."""
    conn = recon_met.get_connection()
    try:
        return {"years": recon_met.list_years(conn)}
    finally:
        conn.close()


@router.get("/mission/{mission_id}")
async def get_mission(mission_id: str):
    """Full decimated observation track for one mission, keyed by
    mission_id (unique across all years/storms) — the same shape the
    hurricanes site's recon archive browser already consumes, plus
    source_url pointing at NOAA's original NetCDF file for anyone who
    wants the full-resolution source instead of our 0.2 Hz decimation."""
    conn = recon_met.get_connection()
    try:
        mission = recon_met.get_mission(conn, mission_id)
        if not mission:
            raise HTTPException(404, f"Unknown mission_id: {mission_id}")
        obs = recon_met.get_observations(conn, mission_id)
        return {
            "mission_id": mission["mission_id"],
            "year": mission["year"],
            "storm_name": mission["storm_name"],
            "storm_id": mission["storm_id"],
            "aircraft": mission["aircraft"],
            "tail_num": mission["tail_num"],
            "flight_date": mission["flight_date"],
            "start_unix": mission["start_unix"],
            "end_unix": mission["end_unix"],
            "source_url": mission["source_url"],
            "obs_count": len(obs),
            "obs": [
                [r["unix_time"], r["lat"], r["lon"], r["wind_kt"], r["wind_dir"], r["sfmr_kt"], r["alt_m"]]
                for r in obs
            ],
        }
    finally:
        conn.close()


@router.get("/mission/{mission_id}/download")
async def download_mission_source(mission_id: str):
    """Streams NOAA's original full-resolution NetCDF file for this
    mission straight through this API — not a redirect. `/mission/{id}`
    above only returns the ~7 fields (lat/lon/wind/SFMR/altitude) this
    project decimates and stores; the raw file has every variable the
    aircraft's instrument suite recorded (attitude, radar, additional
    sensor channels, etc.), which some visualization tools need beyond
    what our JSON exposes. A redirect would leave that up to whether the
    caller's HTTP client follows redirects (not guaranteed for every
    netCDF-consuming tool); streaming the bytes directly works
    everywhere."""
    conn = recon_met.get_connection()
    try:
        mission = recon_met.get_mission(conn, mission_id)
    finally:
        conn.close()
    if not mission or not mission["source_url"]:
        raise HTTPException(404, f"No source file on record for mission_id: {mission_id}")

    source_url = mission["source_url"]
    filename = source_url.rsplit("/", 1)[-1]

    client = httpx.AsyncClient(timeout=120.0, follow_redirects=True)
    try:
        upstream = await client.send(client.build_request("GET", source_url), stream=True)
    except httpx.HTTPError as e:
        await client.aclose()
        raise HTTPException(502, f"Failed to reach source file: {e}") from e
    if upstream.status_code != 200:
        await upstream.aclose()
        await client.aclose()
        raise HTTPException(502, f"Source returned HTTP {upstream.status_code} for {source_url}")

    async def _stream():
        try:
            async for chunk in upstream.aiter_bytes(1 << 16):
                yield chunk
        finally:
            await upstream.aclose()
            await client.aclose()

    headers = {"Content-Disposition": f'attachment; filename="{filename}"'}
    content_length = upstream.headers.get("content-length")
    if content_length:
        headers["Content-Length"] = content_length
    return StreamingResponse(_stream(), media_type="application/x-netcdf", headers=headers)


# NOTE: these two catch-all path routes must be registered AFTER the
# literal-prefixed /mission/* routes above — Starlette matches routes in
# registration order, and {year}/{storm_name} would otherwise greedily
# swallow "/mission/<id>" as year="mission" (failing int validation with a
# confusing 422) before FastAPI ever tries the /mission/* routes.
@router.get("/{year}")
async def list_storms_for_year(year: int):
    """Every storm (plus an 'Unknown / Training' bucket for unidentified
    flights) with archived recon missions in a given year."""
    conn = recon_met.get_connection()
    try:
        rows = recon_met.list_storms_for_year(conn, year)
        if not rows:
            raise HTTPException(404, f"No recon missions found for year {year}.")
        return {
            "year": year,
            "storms": [
                {"storm_name": r["storm_name"], "storm_id": r["storm_id"], "mission_count": r["mission_count"]}
                for r in rows
            ],
        }
    finally:
        conn.close()


@router.get("/{year}/{storm_name:path}")
async def list_missions_for_storm(year: int, storm_name: str):
    """Every archived mission for one storm in one year — enough to
    populate a year -> storm -> mission dropdown chain.

    storm_name uses the `:path` converter (not a plain `{storm_name}`)
    because the "Unknown / Training" bucket name contains a literal "/" —
    Starlette decodes %2F to a real slash before routing, so a plain path
    parameter (which never matches across a "/") 404'd on that bucket no
    matter how the caller encoded it. `:path` allows a slash within this
    segment; safe here since storm_name is the last path segment."""
    conn = recon_met.get_connection()
    try:
        rows = recon_met.list_missions_for_storm(conn, year, storm_name)
        if not rows:
            raise HTTPException(404, f"No recon missions found for {storm_name!r} in {year}.")
        return {"year": year, "storm_name": storm_name, "missions": [_mission_summary(r) for r in rows]}
    finally:
        conn.close()
