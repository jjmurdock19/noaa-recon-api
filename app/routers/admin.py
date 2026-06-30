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
from app.paths import CACHE_ROOT
from app.routers.satellite import VALID_BANDS, VALID_CMAPS, _cache, _nc_cache_dir, _parse_center
from app.services import goes

router = APIRouter(prefix="/admin", tags=["admin"])

MAX_PREFETCH_SLOTS = 500
_prefetch_jobs: dict[str, dict] = {}


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


@router.get("/status", dependencies=[Depends(auth.require_login)])
async def status():
    satellite_stats = _cache.stats()
    nc_stats = _dir_stats(_nc_cache_dir)
    return {
        "healthy": True,
        "cache": {
            "satellite": satellite_stats,
            "goes_nc": nc_stats,
            "total_bytes": satellite_stats["bytes"] + nc_stats["bytes"],
        },
    }


# ── Cache browsing / deletion ───────────────────────────────────────────
@router.get("/cache/satellite", dependencies=[Depends(auth.require_login)])
async def list_satellite_cache():
    entries = []
    for key in _cache.list_keys():
        meta = _cache.get_status(key) or {"status": "unknown"}
        png_path = _cache.output_path(key, "png")
        size = png_path.stat().st_size if png_path.exists() else 0
        json_path = _cache.output_path(key, "json")
        mtime = json_path.stat().st_mtime if json_path.exists() else None
        entries.append({
            "key": key,
            "status": meta.get("status"),
            "band": meta.get("band"),
            "cmap": meta.get("cmap"),
            "satellite": meta.get("satellite"),
            "scan_start": meta.get("scan_start"),
            "center": meta.get("center"),
            "width_km": meta.get("width_km"),
            "size_bytes": size,
            "modified": datetime.datetime.fromtimestamp(mtime, tz=datetime.timezone.utc).isoformat() if mtime else None,
        })
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
