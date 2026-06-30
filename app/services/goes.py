"""GOES ABI L2 CMI archive rendering.

Ported from the hurricanes site's `goes_tile.py` (reprojection formula,
color LUTs, S3 download, gap-fill, PNG render are unchanged). Two additions
over the original:

  1. `resolve_nearest()` finds the ABI-L2-CMIPF scan whose start time is
     closest to an arbitrary requested UTC timestamp (not just "first file
     in this hour"), since CMIPF scans land roughly every 10 minutes — this
     is what gets us real 10-minute resolution instead of hourly buckets.
  2. Everything runs in-process (called from a FastAPI BackgroundTask)
     instead of being shelled out to as a subprocess.

No rasterio, pyproj, boto3, satpy, or metpy required.
"""
from __future__ import annotations

import datetime
import logging
import math
import os
import re
import urllib.error
import urllib.request
import xml.etree.ElementTree as ET
from dataclasses import dataclass
from pathlib import Path

import numpy as np
from PIL import Image

log = logging.getLogger("noaa_recon_api.goes")

# ── Temperature → colormap index ────────────────────────────────────────────
TEMP_MIN_K = 160.0
TEMP_MAX_K = 315.0


def _i2t(idx):
    return TEMP_MAX_K - (idx / 255.0) * (TEMP_MAX_K - TEMP_MIN_K)


def _t2i(temp_k):
    clamped = np.clip(temp_k, TEMP_MIN_K, TEMP_MAX_K)
    return np.round((TEMP_MAX_K - clamped) / (TEMP_MAX_K - TEMP_MIN_K) * 255).astype(np.uint8)


def _lerp(t, t0, t1, v0, v1):
    return int(np.clip(np.interp(t, [t0, t1], [v0, v1]), 0, 255))


def _build_lut(fn):
    lut = np.zeros((256, 3), dtype=np.uint8)
    for i in range(256):
        lut[i] = fn(_i2t(i))
    return lut


def _bd(t):
    if t >= 241:
        return [_lerp(t, 241, TEMP_MAX_K, 200, 0)] * 3
    elif t >= 220:
        return [_lerp(t, 220, 241, 255, 200)] * 3
    elif t >= 210:
        return [0, _lerp(t, 210, 220, 30, 140), 255]
    elif t >= 200:
        return [0, 0, _lerp(t, 200, 210, 180, 255)]
    elif t >= 190:
        return [_lerp(t, 190, 200, 150, 0), 0, 210]
    else:
        return [_lerp(t, TEMP_MIN_K, 190, 255, 150), 0, _lerp(t, TEMP_MIN_K, 190, 0, 210)]


def _enhanced(t):
    if t >= 260:
        return [_lerp(t, 260, TEMP_MAX_K, 160, 20)] * 3
    elif t >= 230:
        return [_lerp(t, 230, 260, 255, 160)] * 3
    elif t >= 200:
        return [_lerp(t, 200, 230, 0, 255), _lerp(t, 200, 230, 0, 255), 255]
    else:
        return [_lerp(t, TEMP_MIN_K, 200, 255, 0), 0, _lerp(t, TEMP_MIN_K, 200, 0, 255)]


def _nrl(t):
    if t >= 273:
        return [_lerp(t, 273, TEMP_MAX_K, 80, 5)] * 3
    elif t >= 253:
        return [_lerp(t, 253, 273, 130, 80)] * 3
    elif t >= 233:
        return [_lerp(t, 233, 253, 255, 130)] * 3
    elif t >= 220:
        return [255, 255, _lerp(t, 220, 233, 0, 255)]
    elif t >= 210:
        return [_lerp(t, 210, 220, 0, 255), 255, _lerp(t, 210, 220, 255, 0)]
    elif t >= 200:
        return [0, _lerp(t, 200, 210, 80, 255), 255]
    elif t >= 185:
        return [_lerp(t, 185, 200, 200, 0), 0, 255]
    else:
        return [255, _lerp(t, TEMP_MIN_K, 185, 100, 0), _lerp(t, TEMP_MIN_K, 185, 0, 200)]


def _grayscale(t):
    v = _lerp(t, TEMP_MIN_K, TEMP_MAX_K, 255, 0)
    return [v, v, v]


LUTS = {
    "bd": _build_lut(_bd),
    "enhanced": _build_lut(_enhanced),
    "nrl": _build_lut(_nrl),
    "grayscale": _build_lut(_grayscale),
}


# ── ABI Fixed Grid → geographic lat/lon (PUG Volume 5, Section 4.2) ─────────
def abi_to_latlon(x_rad, y_rad, sat_lon_deg, h, r_eq, r_pol):
    H = r_eq + h
    lam0 = math.radians(sat_lon_deg)

    a1 = np.sin(x_rad) ** 2 + np.cos(x_rad) ** 2 * (
        np.cos(y_rad) ** 2 + (r_eq / r_pol) ** 2 * np.sin(y_rad) ** 2
    )
    b1 = -2.0 * H * np.cos(x_rad) * np.cos(y_rad)
    c1 = H**2 - r_eq**2

    disc = b1**2 - 4.0 * a1 * c1
    on_disk = disc >= 0.0

    rs = np.where(on_disk, (-b1 - np.sqrt(np.maximum(disc, 0.0))) / (2.0 * a1), np.nan)

    # PUG Vol 5 Sec 4.2 defines Sx = H - rs*cos(x)*cos(y) (not the reverse) —
    # lon depends on the *sign* of Sx (via atan2), so getting this backwards
    # rotates every computed longitude by 180 degrees. lat is unaffected
    # since it only uses Sx**2.
    Sx = H - rs * np.cos(x_rad) * np.cos(y_rad)
    Sy = -rs * np.sin(x_rad)
    Sz = rs * np.cos(x_rad) * np.sin(y_rad)

    lat_rad = np.arctan((r_eq / r_pol) ** 2 * Sz / np.sqrt(Sx**2 + Sy**2))
    lon_rad = lam0 - np.arctan2(Sy, Sx)

    lat_deg = np.where(on_disk, np.degrees(lat_rad), np.nan)
    lon_deg = np.where(on_disk, np.degrees(lon_rad), np.nan)
    return lon_deg, lat_deg


# ── NOAA S3 helpers (plain urllib + XML, public bucket, no auth) ───────────
S3_NS = "http://s3.amazonaws.com/doc/2006-03-01/"

# ABI filenames embed scan start as `_sYYYYDDDHHMMSSf_` (f = tenths of a second).
_SCAN_START_RE = re.compile(r"_s(\d{4})(\d{3})(\d{2})(\d{2})(\d{2})\d_")


def _get_satellite_bucket(date: datetime.date) -> tuple[int, str]:
    """Operational GOES-East satellite/bucket for a given date."""
    if date >= datetime.date(2025, 1, 14):
        return 19, "noaa-goes19"
    return 16, "noaa-goes16"


def _parse_scan_start(key: str) -> datetime.datetime | None:
    m = _SCAN_START_RE.search(key)
    if not m:
        return None
    year, doy, hh, mm, ss = (int(g) for g in m.groups())
    return datetime.datetime(year, 1, 1, tzinfo=datetime.timezone.utc) + datetime.timedelta(
        days=doy - 1, hours=hh, minutes=mm, seconds=ss
    )


def list_s3_prefix(bucket: str, prefix: str) -> list[str]:
    url = f"https://{bucket}.s3.amazonaws.com/?list-type=2&prefix={prefix}&max-keys=100"
    req = urllib.request.Request(url, headers={"User-Agent": "noaa-recon-api/0.1"})
    try:
        with urllib.request.urlopen(req, timeout=20) as resp:
            xml_data = resp.read()
    except urllib.error.URLError as e:
        raise RuntimeError(f"S3 list failed: {e}") from e

    tree = ET.fromstring(xml_data)
    ns = {"s3": S3_NS}
    return [el.find("s3:Key", ns).text for el in tree.findall("s3:Contents", ns)]


def download_file(bucket: str, key: str, local_path: Path) -> None:
    url = f"https://{bucket}.s3.amazonaws.com/{key}"
    log.info("Downloading %s", url)
    req = urllib.request.Request(url, headers={"User-Agent": "noaa-recon-api/0.1"})
    with urllib.request.urlopen(req, timeout=180) as resp, open(local_path, "wb") as fout:
        while True:
            chunk = resp.read(1 << 17)  # 128 KB
            if not chunk:
                break
            fout.write(chunk)
    log.info("Saved -> %s (%d KB)", local_path, os.path.getsize(local_path) // 1024)


@dataclass
class ResolvedScan:
    bucket: str
    key: str
    satellite: int
    band: int
    scan_start: datetime.datetime


def resolve_nearest(target: datetime.datetime, band: int) -> ResolvedScan:
    """Find the ABI-L2-CMIPF scan for `band` whose start time is nearest
    `target` (UTC), searching the target's hour and the following hour
    (CMIPF scans land roughly every 10 minutes and can cross an hour
    boundary relative to the requested minute)."""
    if target.tzinfo is None:
        target = target.replace(tzinfo=datetime.timezone.utc)

    this_hour = target.replace(minute=0, second=0, microsecond=0)
    next_hour = this_hour + datetime.timedelta(hours=1)
    chan = f"C{band:02d}"

    candidates: list[tuple[str, str, datetime.datetime]] = []
    for hour_dt in (this_hour, next_hour):
        satellite, bucket = _get_satellite_bucket(hour_dt.date())
        prefix = f"ABI-L2-CMIPF/{hour_dt.year}/{hour_dt.timetuple().tm_yday:03d}/{hour_dt.hour:02d}/"
        try:
            keys = list_s3_prefix(bucket, prefix)
        except RuntimeError:
            continue
        for k in keys:
            if chan not in k or not k.endswith(".nc"):
                continue
            scan_start = _parse_scan_start(k)
            if scan_start is None:
                continue
            candidates.append((bucket, k, scan_start))

    if not candidates:
        raise FileNotFoundError(
            f"No GOES Band {band} scan found near {target.isoformat()} "
            f"(searched {this_hour.isoformat()} and {next_hour.isoformat()})"
        )

    bucket, key, scan_start = min(candidates, key=lambda c: abs((c[2] - target).total_seconds()))
    satellite, _ = _get_satellite_bucket(scan_start.date())
    return ResolvedScan(bucket=bucket, key=key, satellite=satellite, band=band, scan_start=scan_start)


def ensure_downloaded(resolved: ResolvedScan, nc_cache_dir: Path) -> Path:
    nc_cache_dir.mkdir(parents=True, exist_ok=True)
    local_path = nc_cache_dir / os.path.basename(resolved.key)
    if local_path.exists():
        log.info("Cache hit: %s", local_path.name)
    else:
        download_file(resolved.bucket, resolved.key, local_path)
    return local_path


# ── Core render ──────────────────────────────────────────────────────────
def fill_gaps(data: np.ndarray, iterations: int = 6) -> np.ndarray:
    """Fill NaN holes via nearest-neighbor expansion (4-directional, N passes)."""
    result = data.copy()
    for _ in range(iterations):
        for dy, dx in [(-1, 0), (1, 0), (0, -1), (0, 1)]:
            shifted = np.roll(np.roll(result, dy, axis=0), dx, axis=1)
            mask = np.isnan(result) & np.isfinite(shifted)
            result[mask] = shifted[mask]
    return result


def render_to_png(
    nc_path: Path,
    cmap_name: str,
    out_png: Path,
    out_size: int = 2048,
    downsample_step: int | None = None,
) -> dict:
    """Read GOES ABI L2 CMI NetCDF, reproject to EPSG:4326, apply the color
    LUT, save a georeferenced RGBA PNG. Returns bounds/metadata."""
    import netCDF4 as nc4

    log.info("Reading %s", nc_path)
    ds = nc4.Dataset(str(nc_path), "r")
    try:
        cmi_raw = ds.variables["CMI"][:]
        x_rad = ds.variables["x"][:]
        y_rad = ds.variables["y"][:]

        proj = ds.variables["goes_imager_projection"]
        sat_lon = float(proj.longitude_of_projection_origin)
        h = float(proj.perspective_point_height)
        r_eq = float(proj.semi_major_axis)
        r_pol = float(proj.semi_minor_axis)
    finally:
        ds.close()

    ny, nx = cmi_raw.shape
    if downsample_step is None:
        downsample_step = max(1, max(ny, nx) // 2160)
    step = downsample_step
    log.info("Source %dx%d, downsample step=%d -> %dx%d", ny, nx, step, ny // step, nx // step)

    x_ds = x_rad[::step]
    y_ds = y_rad[::step]
    if isinstance(cmi_raw, np.ma.MaskedArray):
        cmi_ds = cmi_raw[::step, ::step].filled(np.nan).astype(np.float32)
    else:
        cmi_ds = cmi_raw[::step, ::step].astype(np.float32)

    XX, YY = np.meshgrid(x_ds, y_ds)
    LON, LAT = abi_to_latlon(XX, YY, sat_lon, h, r_eq, r_pol)

    lat_S, lat_N = -81.3, 81.3
    lon_W, lon_E = sat_lon - 81.0, sat_lon + 81.0

    output = np.full((out_size, out_size), np.nan, dtype=np.float32)
    col = ((LON - lon_W) / (lon_E - lon_W) * out_size).astype(np.int32)
    row = ((lat_N - LAT) / (lat_N - lat_S) * out_size).astype(np.int32)

    valid = (
        np.isfinite(LON)
        & np.isfinite(LAT)
        & np.isfinite(cmi_ds)
        & (col >= 0)
        & (col < out_size)
        & (row >= 0)
        & (row < out_size)
    )
    output[row[valid], col[valid]] = cmi_ds[valid]
    log.info("Painted %d / %d source pixels", int(valid.sum()), int(valid.size))

    output = fill_gaps(output, iterations=6)

    lut = LUTS.get(cmap_name, LUTS["bd"])
    idx = _t2i(output)
    good = np.isfinite(output)

    rgba = np.zeros((out_size, out_size, 4), dtype=np.uint8)
    rgba[good, 0] = lut[idx[good], 0]
    rgba[good, 1] = lut[idx[good], 1]
    rgba[good, 2] = lut[idx[good], 2]
    rgba[good, 3] = 220
    rgba[~good, 3] = 0

    out_png.parent.mkdir(parents=True, exist_ok=True)
    Image.fromarray(rgba, "RGBA").save(str(out_png), optimize=False)
    log.info("Saved PNG: %s", out_png)

    return {"bounds": [[lat_S, lon_W], [lat_N, lon_E]], "sat_lon": round(sat_lon, 1)}


def render_and_store(resolved: ResolvedScan, cmap_name: str, key: str, nc_cache_dir: Path, cache) -> None:
    """Entry point for a FastAPI BackgroundTask: download (if needed),
    render, and write the result into the shared ResultCache."""
    try:
        nc_path = ensure_downloaded(resolved, nc_cache_dir)
        out_png = cache.output_path(key, "png")
        render_meta = render_to_png(nc_path, cmap_name, out_png)
        meta = {
            "status": "ready",
            "key": key,
            "png_url": f"/cache/satellite/{key}.png",
            "bounds": render_meta["bounds"],
            "band": resolved.band,
            "cmap": cmap_name,
            "satellite": f"GOES-{resolved.satellite}",
            "sat_lon": render_meta["sat_lon"],
            "scan_start": resolved.scan_start.isoformat(),
        }
        cache.write_result(key, meta)
    except Exception as e:  # noqa: BLE001 - report all failures to the client via cache
        log.exception("GOES render failed for key=%s", key)
        cache.write_result(key, {"status": "error", "key": key, "message": str(e)})
