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

VALID_CMAPS = set(goes.LUTS.keys()) | set(goes.STOPS_BY_CMAP.keys()) | goes.REFLECTANCE_CMAPS | {"default"}
VALID_BANDS = {5, 7, 9, 13}  # Band 2 standalone / TDR remain follow-up phases
VALID_PRODUCTS = {"sandwich", "geocolor"}
NM_PER_KM = 1.0 / 1.852

BAND_NAMES = {
    5: "Near-IR (Snow/Ice), 1.6µm",
    7: "Shortwave IR (\"Fire Temperature\"), 3.9µm",
    9: "Mid-Level Water Vapor, 6.9µm",
    13: "Clean IR Window, 10.3µm",
}

# Presentation-layer satellite coverage — GOES-16 East operational start,
# GOES-17 West operational start (both from NOAA's own commissioning
# announcements; approximate to the day). Cutover dates match
# goes._get_satellite_bucket() exactly (kept in sync manually — that
# function is the actual source of truth for which bucket a date resolves
# to; this is just for /products' human-readable summary).
SATELLITE_COVERAGE = {
    "goes-east": [
        {"satellite": "GOES-16", "start": "2017-12-18", "end": "2025-01-14"},
        {"satellite": "GOES-19", "start": "2025-01-14", "end": None},
    ],
    "goes-west": [
        {"satellite": "GOES-17", "start": "2019-02-12", "end": "2023-01-10"},
        {"satellite": "GOES-18", "start": "2023-01-10", "end": None},
    ],
}


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
    band: int = Query(13, description="13 = Clean IR, 9 = Water Vapor, 7 = Shortwave IR, 5 = Near-IR Snow/Ice. Ignored if `product` is given."),
    cmap: str = Query(
        "default",
        description="default | abi13 | abi9 | abi7 | abi5 | bd | ir4 | enhanced | nrl | grayscale. "
        "'default' resolves to the correct per-band standard enhancement — bands are different physical "
        "quantities (temperature vs. reflectance) and aren't interchangeable. Ignored if `product` is given.",
    ),
    product: Optional[str] = Query(
        None,
        description="'sandwich' (Band 13 IR x Band 2 VIS blend) or 'geocolor' (simplified day/night true-color "
        "+ IR composite — see API.md, this is NOT NOAA's proprietary GeoColor). When given, `band`/`cmap` are "
        "ignored. Full-disk only — bbox (`center`/`dims`) isn't supported for composites yet.",
    ),
    satellite: str = Query(
        "goes-east",
        description="'goes-east' (GOES-16 until 2025-01-14, then GOES-19) or 'goes-west' "
        "(GOES-17 until 2023-01-10, then GOES-18). Both only cover ABI-era dates (~2017-2018 "
        "onward) — pre-ABI satellites used a different instrument/format with no open archive.",
    ),
    center: Optional[str] = Query(
        None, description="'lat,lon' — render only a box around this point instead of the full disk. Requires `dims`. Not supported with `product`."
    ),
    dims: Optional[float] = Query(
        None, description="Full width/height of the bounding box, centered on `center`. Requires `center`."
    ),
    unit: str = Query("nm", description="Unit for `dims`: 'nm' (nautical miles) or 'km'"),
    resolution_km: Optional[float] = Query(
        None,
        description="km per output pixel for a bbox request. Omit for the sensor's native resolution "
        "(highest detail — ~2km for most bands, 1km for band 5). Increase to render faster / smaller files.",
    ),
):
    if satellite not in ("goes-east", "goes-west"):
        raise HTTPException(400, "satellite must be 'goes-east' or 'goes-west'")
    sat_side = "west" if satellite == "goes-west" else "east"
    if unit not in ("nm", "km"):
        raise HTTPException(400, "unit must be 'nm' or 'km'")
    if (center is None) != (dims is None):
        raise HTTPException(400, "center and dims must be provided together (omit both for a full-disk render)")

    if product is not None:
        if product not in VALID_PRODUCTS:
            raise HTTPException(400, f"product must be one of {sorted(VALID_PRODUCTS)}")
        if center is not None:
            raise HTTPException(400, "center/dims (bbox) aren't supported for composite products yet — full-disk only")

        try:
            resolved_ir = goes.resolve_nearest(time, 13, sat_side)
        except FileNotFoundError as e:
            raise HTTPException(404, str(e)) from e

        key = f"goes_{product}_{resolved_ir.satellite}_{resolved_ir.scan_start.strftime('%Y%m%dT%H%M%S')}"
        status = _cache.get_status(key)
        if status:
            return status
        lock_params = {
            "product": product,
            "satellite": f"GOES-{resolved_ir.satellite}",
            "scan_start": resolved_ir.scan_start.isoformat(),
        }
        _cache.acquire_lock(key, lock_params)
        background_tasks.add_task(goes.render_product_and_store, product, resolved_ir, key, _nc_cache_dir, _cache)
        return {"status": "generating", "key": key, **lock_params}

    if band not in VALID_BANDS:
        raise HTTPException(400, f"band must be one of {sorted(VALID_BANDS)}")
    if cmap not in VALID_CMAPS:
        raise HTTPException(400, f"cmap must be one of {sorted(VALID_CMAPS)}")
    if cmap == "default":
        cmap = goes.DEFAULT_CMAP_BY_BAND[band]

    bbox = None
    if center is not None:
        lat, lon = _parse_center(center)
        width_km = dims if unit == "km" else dims * 1.852
        try:
            bbox = goes.resolve_bbox_request(lat, lon, width_km, resolution_km, band)
        except ValueError as e:
            raise HTTPException(400, str(e)) from e

    try:
        resolved = goes.resolve_nearest(time, band, sat_side)
    except FileNotFoundError as e:
        raise HTTPException(404, str(e)) from e

    key = f"goes_{band}_{cmap}_{resolved.satellite}_{resolved.scan_start.strftime('%Y%m%dT%H%M%S')}"
    if bbox is not None:
        key += f"_c{bbox.center_lat:.3f}_{bbox.center_lon:.3f}_w{bbox.width_km:.0f}_r{bbox.resolution_km:.1f}"

    status = _cache.get_status(key)
    if status:
        return status

    lock_params = {
        "band": band,
        "cmap": cmap,
        "satellite": f"GOES-{resolved.satellite}",
        "scan_start": resolved.scan_start.isoformat(),
    }
    if bbox is not None:
        lock_params["center"] = [bbox.center_lat, bbox.center_lon]
        lock_params["width_km"] = bbox.width_km
    _cache.acquire_lock(key, lock_params)
    background_tasks.add_task(goes.render_and_store, resolved, cmap, key, _nc_cache_dir, _cache, bbox)
    return {"status": "generating", "key": key, **lock_params}


@router.get("/status/{key}", response_model=TileStatus)
async def get_status(key: str):
    status = _cache.get_status(key)
    if status is None:
        return {"status": "idle"}
    return status


def _rgb_to_hex(rgb) -> str:
    return "#{:02X}{:02X}{:02X}".format(*[int(round(v)) for v in rgb])


@router.get("/colortable")
async def get_colortable(
    cmap: str = Query("default", description="Same values as GET /tile's cmap param"),
    band: int = Query(13, description="Used to resolve cmap='default'"),
):
    """Returns the exact color stops for a colortable, so a client can
    render a legend that's guaranteed to match what /tile actually
    produces (single source of truth — see STOPS_BY_CMAP / LUTS in
    app/services/goes.py)."""
    if cmap not in VALID_CMAPS:
        raise HTTPException(400, f"cmap must be one of {sorted(VALID_CMAPS)}")
    if cmap == "default":
        if band not in VALID_BANDS:
            raise HTTPException(400, f"band must be one of {sorted(VALID_BANDS)}")
        cmap = goes.DEFAULT_CMAP_BY_BAND[band]

    if cmap in goes.REFLECTANCE_CMAPS:
        # Reflectance bands have no "temperature" — the legend is a plain
        # 0-100% grayscale ramp through the same gamma stretch the
        # renderer applies (see goes._reflectance_gray).
        stops = [
            {"reflectance_pct": pct, "hex": _rgb_to_hex([goes._reflectance_gray(pct / 100.0)] * 3)}
            for pct in range(0, 101, 10)
        ]
        return {"cmap": cmap, "unit": "%", "exact": True, "stops": stops}

    if cmap in goes.STOPS_BY_CMAP:
        stops = [{"temp_c": t, "hex": _rgb_to_hex(rgb)} for t, rgb in goes.STOPS_BY_CMAP[cmap]]
        exact = True
    else:
        # LUT-based colortables: sample at evenly spaced indices for a
        # representative (not exhaustive) legend.
        lut = goes.LUTS[cmap]
        sample_indices = range(0, 256, 16)
        stops = [{"temp_c": round(goes._i2t(i) - 273.15, 1), "hex": _rgb_to_hex(lut[i])} for i in sample_indices]
        stops.sort(key=lambda s: s["temp_c"])
        exact = False

    return {"cmap": cmap, "unit": "C", "exact": exact, "stops": stops}


@router.get("/products")
async def list_products():
    """Discovery endpoint: every single-band and composite product this
    API can render, plus which UTC date range each requires satellite is
    actually available for. Meant for building a product picker without
    hardcoding this project's band/cmap/coverage knowledge client-side."""
    bands = [
        {
            "band": band,
            "name": BAND_NAMES[band],
            "kind": "reflectance" if band in goes.REFLECTANCE_BANDS else "brightness_temp",
            "default_cmap": goes.DEFAULT_CMAP_BY_BAND[band],
            "cmaps": sorted(
                {goes.DEFAULT_CMAP_BY_BAND[band]}
                | (set() if band in goes.REFLECTANCE_BANDS else set(goes.LUTS.keys()))
            ),
            "native_resolution_km": goes.NATIVE_GSD_KM.get(band),
            "bbox_supported": True,
        }
        for band in sorted(VALID_BANDS)
    ]
    products = [
        {
            "product": "sandwich",
            "name": "IR/VIS Sandwich",
            "description": "Band 13 IR colorized with the abi13 enhancement, modulated by Band 2 visible "
            "brightness to show convective texture. Falls back to darkened plain IR at night (no visible signal).",
            "bbox_supported": False,
        },
        {
            "product": "geocolor",
            "name": "GeoColor-style composite (approximate)",
            "description": "Simplified day/night composite: synthetic true color (Bands 1/2/3, CIRA synthetic-"
            "green recipe) by day, abi13 colorized IR by night, blended by solar zenith angle near the "
            "terminator. NOT NOAA/CIRA's proprietary GeoColor — no city lights layer, no atmospheric "
            "(Rayleigh) correction.",
            "bbox_supported": False,
        },
    ]
    return {"bands": bands, "products": products, "satellites": SATELLITE_COVERAGE}
