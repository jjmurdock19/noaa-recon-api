import datetime
from typing import Optional

from fastapi import APIRouter, BackgroundTasks, HTTPException, Query

from app.models import TileStatus
from app.services import goes
from app.services.cache import ResultCache
from app.paths import CACHE_ROOT

router = APIRouter(prefix="/satellite", tags=["satellite"])

_cache = ResultCache(CACHE_ROOT / "satellite")
_nc_cache_dir = CACHE_ROOT / "goes_nc"

VALID_CMAPS = set(goes.LUTS.keys())
VALID_BANDS = {9, 13}  # Band 2 (visible) / GeoColor are follow-up phases
NM_PER_KM = 1.0 / 1.852


def _parse_center(center: str) -> tuple[float, float]:
    parts = center.split(",")
    if len(parts) != 2:
        raise HTTPException(400, "center must be 'lat,lon', e.g. '25.5,-80.3'")
    try:
        lat, lon = float(parts[0].strip()), float(parts[1].strip())
    except ValueError as e:
        raise HTTPException(400, "center must be 'lat,lon' with numeric values") from e
    return lat, lon


@router.get("/tile", response_model=TileStatus)
async def get_tile(
    background_tasks: BackgroundTasks,
    time: datetime.datetime = Query(..., description="UTC timestamp, e.g. 2024-09-28T12:00:00Z"),
    band: int = Query(13, description="13 = Clean IR, 9 = Water Vapor"),
    cmap: str = Query("bd", description="bd | enhanced | nrl | grayscale | ir4"),
    satellite: str = Query("goes-east", description="Only 'goes-east' is implemented currently"),
    center: Optional[str] = Query(
        None, description="'lat,lon' — render only a box around this point instead of the full disk. Requires `dims`."
    ),
    dims: Optional[float] = Query(
        None, description="Full width/height of the bounding box, centered on `center`. Requires `center`."
    ),
    unit: str = Query("nm", description="Unit for `dims`: 'nm' (nautical miles) or 'km'"),
    resolution_km: Optional[float] = Query(
        None,
        description="km per output pixel for a bbox request. Omit for the sensor's native resolution "
        "(highest detail — ~2km for bands 9/13). Increase to render faster / smaller files.",
    ),
):
    if satellite != "goes-east":
        raise HTTPException(400, "Only satellite='goes-east' is implemented currently (GOES-West is a follow-up phase)")
    if band not in VALID_BANDS:
        raise HTTPException(400, f"band must be one of {sorted(VALID_BANDS)}")
    if cmap not in VALID_CMAPS:
        raise HTTPException(400, f"cmap must be one of {sorted(VALID_CMAPS)}")
    if unit not in ("nm", "km"):
        raise HTTPException(400, "unit must be 'nm' or 'km'")
    if (center is None) != (dims is None):
        raise HTTPException(400, "center and dims must be provided together (omit both for a full-disk render)")

    bbox = None
    if center is not None:
        lat, lon = _parse_center(center)
        width_km = dims if unit == "km" else dims * 1.852
        try:
            bbox = goes.resolve_bbox_request(lat, lon, width_km, resolution_km, band)
        except ValueError as e:
            raise HTTPException(400, str(e)) from e

    try:
        resolved = goes.resolve_nearest(time, band)
    except FileNotFoundError as e:
        raise HTTPException(404, str(e)) from e

    key = f"goes_{band}_{cmap}_{resolved.satellite}_{resolved.scan_start.strftime('%Y%m%dT%H%M%S')}"
    if bbox is not None:
        key += f"_c{bbox.center_lat:.3f}_{bbox.center_lon:.3f}_w{bbox.width_km:.0f}_r{bbox.resolution_km:.1f}"

    status = _cache.get_status(key)
    if status:
        return status

    _cache.acquire_lock(key)
    background_tasks.add_task(goes.render_and_store, resolved, cmap, key, _nc_cache_dir, _cache, bbox)
    return {"status": "generating", "key": key}


@router.get("/status/{key}", response_model=TileStatus)
async def get_status(key: str):
    status = _cache.get_status(key)
    if status is None:
        return {"status": "idle"}
    return status
