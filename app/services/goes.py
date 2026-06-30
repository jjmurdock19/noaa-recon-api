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


# ── GOES IR4 (true ABI Band 13 standard enhancement) ────────────────────────
# Sourced from satpy's (pytroll/satpy, the standard open-source ABI/AHI
# processing library) `colorized_ir_clouds` enhancement in
# satpy/etc/enhancements/generic.yaml: greyscale from 253.15-303.15K
# (-20C to +30C), colorized 193.15-253.15K (-80C to -20C) using the
# ColorBrewer "Spectral" 11-class diverging palette (colorbrewer2.org),
# coldest=dark red -> warmest-of-band=purple. This is the closest publicly
# documented match to the "color IR" enhancement used on NOAA/STAR's GOES
# Image Viewer and most public satellite loops, rather than an in-house
# approximation like the other LUTs in this module.
_SPECTRAL_11 = [
    (158, 1, 66),     # coldest: dark red    #9e0142
    (213, 62, 79),    #          red         #d53e4f
    (244, 109, 67),   #          orange-red  #f46d43
    (253, 174, 97),   #          orange      #fdae61
    (254, 224, 139),  #          pale orange #fee08b
    (255, 255, 191),  #          pale yellow #ffffbf
    (230, 245, 152),  #          yellow-green#e6f598
    (171, 221, 164),  #          light green #abdda4
    (102, 194, 165),  #          teal        #66c2a5
    (50, 136, 189),   #          blue        #3288bd
    (94, 79, 162),    # warmest-of-band: purple #5e4fa2
]
_IR4_GREY_MIN_K = 253.15   # -20C — boundary between greyscale and color band
_IR4_GREY_MAX_K = 303.15   # +30C — warmest, clipped to black
_IR4_COLOR_MIN_K = 193.15  # -80C — coldest, clipped to dark red


def _spectral_interp(frac):
    frac = min(1.0, max(0.0, frac))
    pos = frac * (len(_SPECTRAL_11) - 1)
    i0 = int(math.floor(pos))
    i1 = min(i0 + 1, len(_SPECTRAL_11) - 1)
    t = pos - i0
    c0, c1 = _SPECTRAL_11[i0], _SPECTRAL_11[i1]
    return [int(round(c0[k] + (c1[k] - c0[k]) * t)) for k in range(3)]


def _goes_ir4(t):
    if t >= _IR4_GREY_MAX_K:
        return [0, 0, 0]
    if t >= _IR4_GREY_MIN_K:
        v = _lerp(t, _IR4_GREY_MIN_K, _IR4_GREY_MAX_K, 255, 0)
        return [v, v, v]
    frac = (t - _IR4_COLOR_MIN_K) / (_IR4_GREY_MIN_K - _IR4_COLOR_MIN_K)
    return _spectral_interp(frac)


# ── "Default ABI" per-band enhancements ─────────────────────────────────────
# Exact temperature(C)->hex stops supplied directly by the project owner for
# this exact use case (one table per band) — these are the standard
# enhancements this project treats as canonical for these two bands, not a
# third-party approximation like `ir4` above. Every stop below is the
# literal source data, not a visual estimate. Original source files:
# docs/colortable_sources/band13_colortable.json, band9_colortable.json.
def _interp_stops(t_c, stops):
    """stops: ascending list of (temp_C, (r,g,b)). Linear interpolation
    between neighboring stops; clamps to the end colors outside the range."""
    if t_c <= stops[0][0]:
        return list(stops[0][1])
    if t_c >= stops[-1][0]:
        return list(stops[-1][1])
    for (t0, c0), (t1, c1) in zip(stops, stops[1:]):
        if t0 <= t_c <= t1:
            frac = (t_c - t0) / (t1 - t0) if t1 != t0 else 0.0
            return [int(round(c0[k] + (c1[k] - c0[k]) * frac)) for k in range(3)]
    return list(stops[-1][1])  # unreachable, satisfies type checkers


# Band 13 (Clean IR Window, 10.3um): white at the most extreme cold
# overshooting tops (-110C), down through black (-80C), a rainbow band
# from -80C to -32C highlighting severe convection, a hard cut to light
# grey at -31C, then greyscale (light=cold, dark=warm) to black at +57C.
_ABI13_STOPS = [
    (-110, (255, 255, 255)),  # white — most extreme overshooting tops
    (-80, (0, 0, 0)),         # black
    (-75, (51, 0, 0)),        # dark maroon
    (-65, (255, 69, 0)),      # orange-red
    (-59, (173, 255, 47)),    # green-yellow
    (-50, (0, 255, 0)),       # green
    (-40, (0, 0, 128)),       # navy
    (-32, (0, 255, 255)),     # cyan — rainbow band ends here
    (-31, (204, 204, 204)),   # hard cut to light grey — greyscale starts
    (-20, (153, 153, 153)),
    (6, (102, 102, 102)),
    (31, (51, 51, 51)),
    (57, (0, 0, 0)),          # black — warmest (clear sky / ground)
]


def _abi13(t_k):
    return _interp_stops(t_k - 273.15, _ABI13_STOPS)


# Band 9 (Mid-Level Water Vapor, 6.9um): cyan at coldest/moist (-93C),
# through green tones, white at the moist/dry transition (-42C), a band
# of purple/navy/near-black indigo (-30C to -18C), then yellow/orange/red
# to black at the warmest/driest (+7C).
_ABI9_STOPS = [
    (-93, (0, 255, 255)),    # cyan — coldest, moist cloud tops
    (-75, (60, 179, 113)),   # medium sea green
    (-54, (120, 171, 120)),  # muted green
    (-42, (255, 255, 255)),  # white — moist/dry transition
    (-30, (153, 153, 204)),  # light purple
    (-24, (0, 0, 128)),      # navy
    (-18, (34, 34, 59)),     # dark indigo
    (-12, (255, 255, 0)),    # yellow
    (-5, (255, 127, 0)),     # orange
    (2, (255, 0, 0)),        # red
    (7, (0, 0, 0)),          # black — warmest, driest
]


def _abi9(t_k):
    return _interp_stops(t_k - 273.15, _ABI9_STOPS)


LUTS = {
    "bd": _build_lut(_bd),
    "enhanced": _build_lut(_enhanced),
    "nrl": _build_lut(_nrl),
    "grayscale": _build_lut(_grayscale),
    "ir4": _build_lut(_goes_ir4),
    # abi13/abi9 are intentionally NOT routed through the shared 256-bucket
    # LUT above (_build_lut quantizes the full 160-315K range into 256
    # steps, ~0.6C/step) — their source data has a deliberate 1C-wide hard
    # cut (Band 13: cyan@-32C -> light grey@-31C) that quantization smears
    # into a muddy blended color, and a range (Band 13 needs up to +57C)
    # wider than the shared LUT's 160-315K (-113..+42C) window, clamping
    # the warm end before it reaches true black. See STOPS_BY_CMAP /
    # _colorize_exact below — these are evaluated exactly, per-pixel,
    # directly from _ABI13_STOPS/_ABI9_STOPS instead.
}

# cmap="default" resolves to one of these based on the requested band, since
# Band 13 (IR window) and Band 9 (water vapor) use genuinely different
# enhancement conventions — there's no single "default" LUT independent of
# which physical quantity is being displayed.
DEFAULT_CMAP_BY_BAND = {13: "abi13", 9: "abi9"}

# Colortables evaluated exactly (vectorized, full float precision) rather
# than through the shared quantized LUT — see comment above LUTS.
STOPS_BY_CMAP = {"abi13": _ABI13_STOPS, "abi9": _ABI9_STOPS}


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


# ── Bounding-box requests ───────────────────────────────────────────────────
# Native ground sample distance for the "2km" ABI bands this service handles.
# Used as the default (highest-fidelity) render resolution for a bbox request,
# and as the floor for the `resolution_km` coarsening param.
NATIVE_GSD_KM = {9: 2.0, 13: 2.0}

KM_PER_DEG_LAT = 111.32
MIN_BBOX_WIDTH_KM = 10.0
MAX_BBOX_WIDTH_KM = 8000.0
MIN_OUT_SIZE = 64
MAX_OUT_SIZE = 4096
# Sparse grid for the cheap "locate" pass before cropping to native resolution.
# 160x160 = 25,600 points vs. ~4.6M for the old full-disk coarse pass — this
# is what actually makes a bbox request faster to *process* than a full-disk
# one (the S3 download itself is unchanged; see README "Known limitations").
LOCATE_GRID = 160


@dataclass
class BBoxRequest:
    center_lat: float
    center_lon: float
    width_km: float
    resolution_km: float


def resolve_bbox_request(
    center_lat: float, center_lon: float, width_km: float, resolution_km: float | None, band: int
) -> BBoxRequest:
    """Validate and clamp a bbox request's parameters."""
    if not (-90.0 <= center_lat <= 90.0):
        raise ValueError(f"center latitude {center_lat} out of range [-90, 90]")
    if not (-180.0 <= center_lon <= 180.0):
        raise ValueError(f"center longitude {center_lon} out of range [-180, 180]")
    width_km = float(np.clip(width_km, MIN_BBOX_WIDTH_KM, MAX_BBOX_WIDTH_KM))

    native = NATIVE_GSD_KM.get(band, 2.0)
    if resolution_km is None:
        resolution_km = native
    else:
        # Can't resolve finer than the sensor's native pixel size.
        resolution_km = max(float(resolution_km), native)

    return BBoxRequest(center_lat=center_lat, center_lon=center_lon, width_km=width_km, resolution_km=resolution_km)


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


def _smooth(output: np.ndarray, passes: int = 1) -> np.ndarray:
    """NaN-aware 3x3 box blur (a couple of passes approximates a mild
    Gaussian). This is anti-aliasing over real sampled data, not invented
    detail — bands 9/13 are physically captured at ~2km/pixel by the ABI
    sensor, which is a hardware ceiling no amount of processing changes.
    What this *does* fix is the blocky look from the forward-projection
    paint step (each source pixel scattered to its nearest output cell,
    leaving hard edges where one source sample dominates several output
    pixels). Only blurs within already-valid (non-NaN) cells; never bleeds
    valid data into off-disk/no-data regions or vice versa."""
    result = output
    for _ in range(passes):
        valid = np.isfinite(result)
        vals = np.where(valid, result, 0.0)
        weight = valid.astype(np.float32)
        vsum = np.zeros_like(result, dtype=np.float32)
        wsum = np.zeros_like(result, dtype=np.float32)
        for dy in (-1, 0, 1):
            for dx in (-1, 0, 1):
                vsum += np.roll(np.roll(vals, dy, axis=0), dx, axis=1)
                wsum += np.roll(np.roll(weight, dy, axis=0), dx, axis=1)
        with np.errstate(invalid="ignore", divide="ignore"):
            blurred = vsum / wsum
        result = np.where(valid, blurred, result)
    return result


def _apply_stops_exact(output_k: np.ndarray, stops: list) -> np.ndarray:
    """Vectorized, full-float-precision version of _interp_stops applied to
    a whole array at once — exact colors, no LUT-quantization smearing.
    `stops` ascending (temp_C, (r,g,b)); `output_k` in Kelvin."""
    output_c = output_k - 273.15
    temps = np.array([s[0] for s in stops], dtype=np.float64)
    rgb = np.array([s[1] for s in stops], dtype=np.float64)
    out = np.empty(output_c.shape + (3,), dtype=np.float64)
    for ch in range(3):
        out[..., ch] = np.interp(output_c, temps, rgb[:, ch])
    return out


def _colorize(output: np.ndarray, cmap_name: str, out_png: Path) -> None:
    good = np.isfinite(output)
    out_size = output.shape[0]
    rgba = np.zeros((out_size, out_size, 4), dtype=np.uint8)

    if cmap_name in STOPS_BY_CMAP:
        rgb = _apply_stops_exact(output, STOPS_BY_CMAP[cmap_name])
        rgba[good, 0] = rgb[good, 0]
        rgba[good, 1] = rgb[good, 1]
        rgba[good, 2] = rgb[good, 2]
    else:
        lut = LUTS.get(cmap_name, LUTS["bd"])
        idx = _t2i(output)
        rgba[good, 0] = lut[idx[good], 0]
        rgba[good, 1] = lut[idx[good], 1]
        rgba[good, 2] = lut[idx[good], 2]

    rgba[good, 3] = 220
    rgba[~good, 3] = 0

    out_png.parent.mkdir(parents=True, exist_ok=True)
    Image.fromarray(rgba, "RGBA").save(str(out_png), optimize=False)
    log.info("Saved PNG: %s", out_png)


def _read_source(nc_path: Path):
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
    return cmi_raw, x_rad, y_rad, sat_lon, h, r_eq, r_pol


def render_to_png(
    nc_path: Path,
    cmap_name: str,
    out_png: Path,
    out_size: int = 2048,
    downsample_step: int | None = None,
) -> dict:
    """Read GOES ABI L2 CMI NetCDF, reproject the full disk to EPSG:4326,
    apply the color LUT, save a georeferenced RGBA PNG. Returns bounds/metadata."""
    cmi_raw, x_rad, y_rad, sat_lon, h, r_eq, r_pol = _read_source(nc_path)

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
    output = _smooth(output)
    _colorize(output, cmap_name, out_png)

    return {"bounds": [[lat_S, lon_W], [lat_N, lon_E]], "sat_lon": round(sat_lon, 1)}


def render_bbox_to_png(nc_path: Path, band: int, cmap_name: str, out_png: Path, bbox: BBoxRequest) -> dict:
    """Like render_to_png, but crops to a center+width bounding box and
    renders at (up to) the sensor's native resolution instead of the full
    disk. Two passes: a cheap sparse "locate" pass finds which slice of the
    source array covers the requested area, then a fine pass reprojects only
    that crop — avoiding the full-disk reprojection cost for a small area."""
    cmi_raw, x_rad, y_rad, sat_lon, h, r_eq, r_pol = _read_source(nc_path)
    ny, nx = cmi_raw.shape

    half_km = bbox.width_km / 2.0
    lat_half_deg = half_km / KM_PER_DEG_LAT
    lon_half_deg = half_km / (KM_PER_DEG_LAT * max(0.01, math.cos(math.radians(bbox.center_lat))))
    lat_S, lat_N = bbox.center_lat - lat_half_deg, bbox.center_lat + lat_half_deg
    lon_W, lon_E = bbox.center_lon - lon_half_deg, bbox.center_lon + lon_half_deg

    # ---- Pass 1: sparse locate ----
    step_y = max(1, ny // LOCATE_GRID)
    step_x = max(1, nx // LOCATE_GRID)
    XXs, YYs = np.meshgrid(x_rad[::step_x], y_rad[::step_y])
    LONs, LATs = abi_to_latlon(XXs, YYs, sat_lon, h, r_eq, r_pol)

    mask = (
        np.isfinite(LONs) & np.isfinite(LATs)
        & (LONs >= lon_W) & (LONs <= lon_E)
        & (LATs >= lat_S) & (LATs <= lat_N)
    )
    if not mask.any():
        raise ValueError(
            f"Requested area ({lat_S:.2f},{lon_W:.2f})-({lat_N:.2f},{lon_E:.2f}) "
            "is outside this scan's visible disk"
        )

    rows_sparse, cols_sparse = np.where(mask)
    pad_y, pad_x = step_y * 2, step_x * 2
    row_lo = max(0, rows_sparse.min() * step_y - pad_y)
    row_hi = min(ny, (rows_sparse.max() + 1) * step_y + pad_y)
    col_lo = max(0, cols_sparse.min() * step_x - pad_x)
    col_hi = min(nx, (cols_sparse.max() + 1) * step_x + pad_x)

    # ---- Pass 2: fine crop at native (or coarsened) resolution ----
    gsd_km = NATIVE_GSD_KM.get(band, 2.0)
    fine_step = max(1, round(bbox.resolution_km / gsd_km))

    x_crop = x_rad[col_lo:col_hi:fine_step]
    y_crop = y_rad[row_lo:row_hi:fine_step]
    cmi_slice = cmi_raw[row_lo:row_hi:fine_step, col_lo:col_hi:fine_step]
    if isinstance(cmi_raw, np.ma.MaskedArray):
        cmi_crop = cmi_slice.filled(np.nan).astype(np.float32)
    else:
        cmi_crop = cmi_slice.astype(np.float32)

    log.info(
        "BBox crop %dx%d native px (rows %d:%d, cols %d:%d, step=%d) for %.0fkm box @ %.1fkm/px",
        cmi_crop.shape[0], cmi_crop.shape[1], row_lo, row_hi, col_lo, col_hi, fine_step,
        bbox.width_km, bbox.resolution_km,
    )

    XX, YY = np.meshgrid(x_crop, y_crop)
    LON, LAT = abi_to_latlon(XX, YY, sat_lon, h, r_eq, r_pol)

    out_size = int(np.clip(round(bbox.width_km / bbox.resolution_km), MIN_OUT_SIZE, MAX_OUT_SIZE))

    output = np.full((out_size, out_size), np.nan, dtype=np.float32)
    col = ((LON - lon_W) / (lon_E - lon_W) * out_size).astype(np.int32)
    row = ((lat_N - LAT) / (lat_N - lat_S) * out_size).astype(np.int32)
    valid = (
        np.isfinite(LON) & np.isfinite(LAT) & np.isfinite(cmi_crop)
        & (col >= 0) & (col < out_size) & (row >= 0) & (row < out_size)
    )
    if not valid.any():
        raise ValueError("Requested area has no valid data in this scan (off-disk or no-data)")
    output[row[valid], col[valid]] = cmi_crop[valid]
    log.info("Painted %d / %d source pixels into %dx%d output", int(valid.sum()), int(valid.size), out_size, out_size)

    output = fill_gaps(output, iterations=6)
    output = _smooth(output)
    _colorize(output, cmap_name, out_png)

    return {
        "bounds": [[lat_S, lon_W], [lat_N, lon_E]],
        "sat_lon": round(sat_lon, 1),
        "resolution_km": bbox.resolution_km,
        "out_size": out_size,
    }


def render_and_store(
    resolved: ResolvedScan, cmap_name: str, key: str, nc_cache_dir: Path, cache, bbox: BBoxRequest | None = None
) -> None:
    """Entry point for a FastAPI BackgroundTask: download (if needed),
    render, and write the result into the shared ResultCache."""
    try:
        nc_path = ensure_downloaded(resolved, nc_cache_dir)
        out_png = cache.output_path(key, "png")
        if bbox is None:
            render_meta = render_to_png(nc_path, cmap_name, out_png)
        else:
            render_meta = render_bbox_to_png(nc_path, resolved.band, cmap_name, out_png, bbox)

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
        if bbox is not None:
            meta["center"] = [bbox.center_lat, bbox.center_lon]
            meta["width_km"] = bbox.width_km
            meta["resolution_km"] = render_meta["resolution_km"]
        cache.write_result(key, meta)
    except Exception as e:  # noqa: BLE001 - report all failures to the client via cache
        log.exception("GOES render failed for key=%s", key)
        cache.write_result(key, {"status": "error", "key": key, "message": str(e)})
