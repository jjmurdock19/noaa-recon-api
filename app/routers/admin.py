"""Admin console API — status, cache browsing/deletion, and bulk prefetch.

Everything here except /login requires an authenticated session (see
app/auth.py). The console frontend (app/console/index.html) is the only
intended caller, but these are just JSON endpoints — nothing stops other
authenticated tooling from using them too.
"""
import datetime
import json
import uuid
from pathlib import Path
from typing import Optional

from fastapi import APIRouter, BackgroundTasks, Depends, HTTPException, Query, Request

from app import auth
from app.logging_config import LOG_FILE
from app.paths import CACHE_ROOT, RECON_MET_DB_PATH, STORMS_DB_PATH
from app.routers.satellite import VALID_BANDS, VALID_CMAPS, _cache, _nc_cache_dir, _parse_center
from app.services import goes, recon_met, stats, storms
from app.services.netcdf_lock import NC_LOCK

router = APIRouter(prefix="/admin", tags=["admin"])

MAX_PREFETCH_SLOTS = 500
_prefetch_jobs: dict[str, dict] = {}

# Singleton (not job-id-keyed, unlike prefetch above) — there's only ever
# one "update the whole archive" job per archive type at a time.
_archive_update_jobs: dict[str, dict] = {
    "storms": {"status": "idle", "started_at": None, "finished_at": None, "summary": None, "error": None},
    "recon_met": {"status": "idle", "started_at": None, "finished_at": None, "summary": None, "error": None},
}


# ── Public status (no login) ────────────────────────────────────────────
# Shown on the console's login screen so anyone can eyeball API health
# before authenticating. Intentionally excludes cache/storage figures —
# those stay behind login in /status below.
@router.get("/public-stats")
async def public_stats():
    return stats.get_public_stats()


# ── Auth ─────────────────────────────────────────────────────────────────
@router.post("/login")
async def login(request: Request):
    body = await request.json()
    username = str(body.get("username", ""))
    password = str(body.get("password", ""))
    if not auth.verify_credentials(username, password):
        raise HTTPException(401, "Invalid username or password")
    request.session["authenticated"] = True
    return {"status": "ok"}


@router.post("/logout")
async def logout(request: Request):
    request.session.clear()
    return {"status": "ok"}


@router.get("/whoami")
async def whoami(request: Request):
    return {"authenticated": auth.is_authenticated(request)}


# ── Status / cache stats ────────────────────────────────────────────────
def _dir_stats(directory: Path) -> dict:
    count, total_bytes = 0, 0
    if directory.exists():
        for p in directory.iterdir():
            if p.is_file():
                count += 1
                total_bytes += p.stat().st_size
    return {"file_count": count, "bytes": total_bytes}


def _db_file_stats(path: Path) -> dict:
    return {"bytes": path.stat().st_size if path.exists() else 0}


@router.get("/status", dependencies=[Depends(auth.require_login)])
async def status():
    satellite_stats = _cache.stats()
    nc_stats = _dir_stats(_nc_cache_dir)
    cache_total = satellite_stats["bytes"] + nc_stats["bytes"]

    storms_conn = storms.get_connection()
    storm_count = storms_conn.execute("SELECT COUNT(*) FROM storms").fetchone()[0]
    storms_conn.close()
    recon_conn = recon_met.get_connection()
    mission_count = recon_conn.execute("SELECT COUNT(*) FROM missions").fetchone()[0]
    recon_conn.close()

    storms_db_stats = {**_db_file_stats(STORMS_DB_PATH), "storm_count": storm_count}
    recon_db_stats = {**_db_file_stats(RECON_MET_DB_PATH), "mission_count": mission_count}
    databases_total = storms_db_stats["bytes"] + recon_db_stats["bytes"]

    return {
        "healthy": True,
        "cache": {
            "satellite": satellite_stats,
            "goes_nc": nc_stats,
            "total_bytes": cache_total,
        },
        "databases": {
            "storms": storms_db_stats,
            "recon_met": recon_db_stats,
            "total_bytes": databases_total,
        },
        "grand_total_bytes": cache_total + databases_total,
    }


# ── Log tail (for the console's live log terminal) ──────────────────────
@router.get("/logs", dependencies=[Depends(auth.require_login)])
async def get_logs(offset: int = Query(0, ge=0), max_bytes: int = Query(65536, ge=1024, le=1_000_000)):
    """Tail app.log for the console's log terminal. `offset` is the byte
    position the caller already has (0 for an initial load, otherwise the
    `offset` a previous call returned) — only bytes past it come back, so
    repeated polling ships new log lines only, not the whole file each time.

    `reset` is set (and the returned text is the last `max_bytes` of the
    file instead of a diff) when there's nothing sensible to diff against:
    the very first load of a file bigger than `max_bytes`, or `offset`
    pointing past the current size because the RotatingFileHandler rotated
    the file out from under it. The console clears its view when it sees
    this instead of appending, since the bytes at a given offset no longer
    mean what they used to.
    """
    if not LOG_FILE.exists():
        return {"text": "", "offset": 0, "reset": True}

    size = LOG_FILE.stat().st_size
    reset = offset > size or (offset == 0 and size > max_bytes)
    start = max(0, size - max_bytes) if reset else offset

    with LOG_FILE.open("rb") as f:
        f.seek(start)
        data = f.read(max_bytes)

    return {
        "text": data.decode("utf-8", errors="replace"),
        "offset": start + len(data),
        "reset": reset,
    }


# ── Cache browsing / deletion ───────────────────────────────────────────
@router.get("/cache/satellite", dependencies=[Depends(auth.require_login)])
async def list_satellite_cache():
    entries = []
    for key in _cache.list_keys():
        meta = dict(_cache.get_status(key) or {"status": "unknown"})
        png_path = _cache.output_path(key, "png")
        size = png_path.stat().st_size if png_path.exists() else 0
        json_path = _cache.output_path(key, "json")
        mtime = json_path.stat().st_mtime if json_path.exists() else None
        # Pass through every field the render pipeline wrote (band, cmap,
        # satellite, sat_lon, scan_start, bounds, center, width_km,
        # resolution_km, png_url, ...) — the console's preview pane shows
        # all of it, so don't hand-curate a subset here.
        meta["key"] = key
        meta["size_bytes"] = size
        meta["modified"] = datetime.datetime.fromtimestamp(mtime, tz=datetime.timezone.utc).isoformat() if mtime else None
        entries.append(meta)
    entries.sort(key=lambda e: e["modified"] or "", reverse=True)
    return {"entries": entries}


@router.delete("/cache/satellite/{key}", dependencies=[Depends(auth.require_login)])
async def delete_satellite_cache_entry(key: str):
    freed = _cache.delete(key)
    return {"status": "ok", "bytes_freed": freed}


@router.delete("/cache/satellite", dependencies=[Depends(auth.require_login)])
async def clear_satellite_cache():
    freed = 0
    for key in _cache.list_keys():
        freed += _cache.delete(key)
    return {"status": "ok", "bytes_freed": freed}


@router.get("/cache/goes_nc", dependencies=[Depends(auth.require_login)])
async def list_goes_nc_cache():
    entries = []
    if _nc_cache_dir.exists():
        for p in sorted(_nc_cache_dir.iterdir(), key=lambda f: f.stat().st_mtime, reverse=True):
            if not p.is_file():
                continue
            scan_start = goes._parse_scan_start(p.name)
            entries.append({
                "filename": p.name,
                "size_bytes": p.stat().st_size,
                "modified": datetime.datetime.fromtimestamp(
                    p.stat().st_mtime, tz=datetime.timezone.utc
                ).isoformat(),
                "scan_start": scan_start.isoformat() if scan_start else None,
            })
    return {"entries": entries}


@router.get("/cache/goes_nc/{filename}/info", dependencies=[Depends(auth.require_login)])
async def get_goes_nc_info(filename: str):
    """Structural metadata for a raw netCDF file — dimensions, variables
    (with shape/dtype/units), and global attributes. netCDF isn't directly
    viewable like an image, so this is the console's "preview" for it,
    analogous to `ncdump -h`."""
    if "/" in filename or ".." in filename:
        raise HTTPException(400, "invalid filename")
    path = _nc_cache_dir / filename
    if not path.exists():
        raise HTTPException(404, "not found")

    import netCDF4 as nc4

    with NC_LOCK:  # see app/services/netcdf_lock.py — HDF5 isn't thread-safe
        ds = nc4.Dataset(str(path), "r")
        try:
            dimensions = {name: len(dim) for name, dim in ds.dimensions.items()}
            variables = []
            for name, var in ds.variables.items():
                variables.append({
                    "name": name,
                    "dimensions": list(var.dimensions),
                    "shape": list(var.shape),
                    "dtype": str(var.dtype),
                    "units": getattr(var, "units", None),
                    "long_name": getattr(var, "long_name", None),
                })
            global_attrs = {attr: str(getattr(ds, attr)) for attr in ds.ncattrs()}
        finally:
            ds.close()

    return {
        "filename": filename,
        "size_bytes": path.stat().st_size,
        "dimensions": dimensions,
        "variables": sorted(variables, key=lambda v: v["name"]),
        "global_attrs": global_attrs,
    }


@router.delete("/cache/goes_nc/{filename}", dependencies=[Depends(auth.require_login)])
async def delete_goes_nc_entry(filename: str):
    if "/" in filename or ".." in filename:
        raise HTTPException(400, "invalid filename")
    p = _nc_cache_dir / filename
    if not p.exists():
        raise HTTPException(404, "not found")
    freed = p.stat().st_size
    p.unlink()
    return {"status": "ok", "bytes_freed": freed}


@router.delete("/cache/goes_nc", dependencies=[Depends(auth.require_login)])
async def clear_goes_nc_cache():
    freed = 0
    if _nc_cache_dir.exists():
        for p in _nc_cache_dir.iterdir():
            if p.is_file():
                freed += p.stat().st_size
                p.unlink()
    return {"status": "ok", "bytes_freed": freed}


# ── Bulk prefetch (load a timeframe into cache) ─────────────────────────
def _run_prefetch_job(job_id: str, timestamps: list, band: int, satellite: str, cmap: str, bbox):
    job = _prefetch_jobs[job_id]
    job["status"] = "running"
    for ts in timestamps:
        try:
            resolved = goes.resolve_nearest(ts, band, satellite)
            resolved_cmap = goes.DEFAULT_CMAP_BY_BAND[band] if cmap == "default" else cmap
            key = f"goes_{band}_{resolved_cmap}_{resolved.satellite}_{resolved.scan_start.strftime('%Y%m%dT%H%M%S')}"
            if bbox is not None:
                key += f"_c{bbox.center_lat:.3f}_{bbox.center_lon:.3f}_w{bbox.width_km:.0f}_r{bbox.resolution_km:.1f}"

            existing = _cache.get_status(key)
            if existing and existing.get("status") == "ready":
                job["skipped"] += 1
            else:
                _cache.acquire_lock(key)
                goes.render_and_store(resolved, resolved_cmap, key, _nc_cache_dir, _cache, bbox)
                result = _cache.get_status(key)
                if result and result.get("status") == "ready":
                    job["completed"] += 1
                else:
                    job["failed"] += 1
                    job["errors"].append(f"{ts.isoformat()}: {(result or {}).get('message', 'unknown error')}")
        except FileNotFoundError as e:
            job["failed"] += 1
            job["errors"].append(f"{ts.isoformat()}: {e}")
        except Exception as e:  # noqa: BLE001 - keep the job loop alive, report and continue
            job["failed"] += 1
            job["errors"].append(f"{ts.isoformat()}: {e}")
        job["processed"] += 1
    job["status"] = "done"
    job["finished_at"] = datetime.datetime.now(datetime.timezone.utc).isoformat()


@router.post("/prefetch", dependencies=[Depends(auth.require_login)])
async def start_prefetch(request: Request, background_tasks: BackgroundTasks):
    body = await request.json()
    try:
        time_start = datetime.datetime.fromisoformat(body["time_start"].replace("Z", "+00:00"))
        time_end = datetime.datetime.fromisoformat(body["time_end"].replace("Z", "+00:00"))
    except (KeyError, ValueError) as e:
        raise HTTPException(400, f"time_start/time_end must be ISO8601: {e}") from e
    interval_minutes = int(body.get("interval_minutes", 60))
    if interval_minutes < 10:
        raise HTTPException(400, "interval_minutes must be >= 10 (CMIPF scans land ~every 10 min)")
    band = int(body.get("band", 13))
    if band not in VALID_BANDS:
        raise HTTPException(400, f"band must be one of {sorted(VALID_BANDS)}")
    satellite = "west" if body.get("satellite") == "goes-west" else "east"
    cmap = body.get("cmap", "default")
    if cmap not in VALID_CMAPS:
        raise HTTPException(400, f"cmap must be one of {sorted(VALID_CMAPS)}")

    bbox = None
    center = body.get("center")
    if center:
        lat, lon = _parse_center(center)
        dims = float(body["dims"])
        unit = body.get("unit", "nm")
        width_km = dims if unit == "km" else dims * 1.852
        try:
            bbox = goes.resolve_bbox_request(lat, lon, width_km, body.get("resolution_km"), band)
        except ValueError as e:
            raise HTTPException(400, str(e)) from e

    if time_end <= time_start:
        raise HTTPException(400, "time_end must be after time_start")
    timestamps = []
    t = time_start
    step = datetime.timedelta(minutes=interval_minutes)
    while t <= time_end:
        timestamps.append(t)
        t += step
    if len(timestamps) > MAX_PREFETCH_SLOTS:
        raise HTTPException(
            400,
            f"That range + interval produces {len(timestamps)} scans, over the {MAX_PREFETCH_SLOTS} limit "
            "— shorten the range or increase interval_minutes.",
        )
    if not timestamps:
        raise HTTPException(400, "time range produced no timestamps")

    job_id = uuid.uuid4().hex[:12]
    _prefetch_jobs[job_id] = {
        "job_id": job_id,
        "status": "queued",
        "total": len(timestamps),
        "processed": 0,
        "completed": 0,
        "skipped": 0,
        "failed": 0,
        "errors": [],
        "started_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "finished_at": None,
    }
    background_tasks.add_task(_run_prefetch_job, job_id, timestamps, band, satellite, cmap, bbox)
    return _prefetch_jobs[job_id]


@router.get("/prefetch/{job_id}", dependencies=[Depends(auth.require_login)])
async def get_prefetch_status(job_id: str):
    job = _prefetch_jobs.get(job_id)
    if not job:
        raise HTTPException(404, "unknown job_id")
    return job


@router.get("/prefetch", dependencies=[Depends(auth.require_login)])
async def list_prefetch_jobs():
    return {"jobs": sorted(_prefetch_jobs.values(), key=lambda j: j["started_at"], reverse=True)}


# ── Force-update the storms / recon MET archives ────────────────────────
# Same nightly-timer code path (storms.run_ingest() / recon_met.run_ingest()),
# just triggerable on demand for data that hasn't been picked up yet by the
# scheduled run — see deploy/storm-archive-update.timer and
# deploy/recon-met-update.timer.
def _run_archive_update(archive: str):
    job = _archive_update_jobs[archive]
    job["status"] = "running"
    job["started_at"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
    job["finished_at"] = None
    job["error"] = None
    try:
        job["summary"] = storms.run_ingest() if archive == "storms" else recon_met.run_ingest()
    except Exception as e:  # noqa: BLE001 - report and let the console show it, don't crash the worker
        job["error"] = str(e)
    finally:
        job["status"] = "done"
        job["finished_at"] = datetime.datetime.now(datetime.timezone.utc).isoformat()


@router.post("/archive-update/{archive}", dependencies=[Depends(auth.require_login)])
async def start_archive_update(archive: str, background_tasks: BackgroundTasks):
    if archive not in _archive_update_jobs:
        raise HTTPException(404, f"Unknown archive: {archive} (expected 'storms' or 'recon_met')")
    job = _archive_update_jobs[archive]
    if job["status"] == "running":
        raise HTTPException(409, f"{archive} update is already running")
    job["status"] = "queued"
    background_tasks.add_task(_run_archive_update, archive)
    return job


@router.get("/archive-update/{archive}", dependencies=[Depends(auth.require_login)])
async def get_archive_update_status(archive: str):
    if archive not in _archive_update_jobs:
        raise HTTPException(404, f"Unknown archive: {archive} (expected 'storms' or 'recon_met')")
    return _archive_update_jobs[archive]
