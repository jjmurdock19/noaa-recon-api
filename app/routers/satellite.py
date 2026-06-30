import datetime

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


@router.get("/tile", response_model=TileStatus)
async def get_tile(
    background_tasks: BackgroundTasks,
    time: datetime.datetime = Query(..., description="UTC timestamp, e.g. 2024-09-28T12:00:00Z"),
    band: int = Query(13, description="13 = Clean IR, 9 = Water Vapor"),
    cmap: str = Query("bd", description="bd | enhanced | nrl | grayscale"),
    satellite: str = Query("goes-east", description="Only 'goes-east' is implemented currently"),
):
    if satellite != "goes-east":
        raise HTTPException(400, "Only satellite='goes-east' is implemented currently (GOES-West is a follow-up phase)")
    if band not in VALID_BANDS:
        raise HTTPException(400, f"band must be one of {sorted(VALID_BANDS)}")
    if cmap not in VALID_CMAPS:
        raise HTTPException(400, f"cmap must be one of {sorted(VALID_CMAPS)}")

    try:
        resolved = goes.resolve_nearest(time, band)
    except FileNotFoundError as e:
        raise HTTPException(404, str(e)) from e

    key = f"goes_{band}_{cmap}_{resolved.satellite}_{resolved.scan_start.strftime('%Y%m%dT%H%M%S')}"

    status = _cache.get_status(key)
    if status:
        return status

    _cache.acquire_lock(key)
    background_tasks.add_task(goes.render_and_store, resolved, cmap, key, _nc_cache_dir, _cache)
    return {"status": "generating", "key": key}


@router.get("/status/{key}", response_model=TileStatus)
async def get_status(key: str):
    status = _cache.get_status(key)
    if status is None:
        return {"status": "idle"}
    return status
