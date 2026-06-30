"""Sanity tests for the GOES satellite path.

Network-dependent tests (S3 listing + download) are skipped unless
NOAA_RECON_API_NETWORK_TESTS=1 is set, so the suite stays fast/offline by
default. The pure-math tests always run.
"""
import datetime
import os

import numpy as np
import pytest

from app.services import goes

NETWORK_TESTS = os.environ.get("NOAA_RECON_API_NETWORK_TESTS") == "1"


def test_luts_are_256x3_uint8():
    for name, lut in goes.LUTS.items():
        assert lut.shape == (256, 3), name
        assert lut.dtype == np.uint8, name


def test_abi_to_latlon_subsatellite_point_is_origin():
    # At x=0, y=0 (boresight), the projected point must be the
    # sub-satellite point: lat=0, lon=sat_lon. This catches the classic
    # "Sx defined with the wrong sign" bug, which silently rotates every
    # computed longitude by 180 degrees (lat is unaffected since it only
    # depends on Sx**2) and makes the renderer paint ~0 pixels onto the
    # output grid because nothing falls inside the expected lon window.
    sat_lon = -75.2
    h = 35786023.0
    r_eq = 6378137.0
    r_pol = 6356752.31414
    lon, lat = goes.abi_to_latlon(np.array([0.0]), np.array([0.0]), sat_lon, h, r_eq, r_pol)
    assert abs(lat[0]) < 1e-6
    assert abs(lon[0] - sat_lon) < 1e-6


def test_abi_to_latlon_nan_off_disk():
    sat_lon = -75.2
    h = 35786023.0
    r_eq = 6378137.0
    r_pol = 6356752.31414
    lon_off, lat_off = goes.abi_to_latlon(np.array([1.0]), np.array([1.0]), sat_lon, h, r_eq, r_pol)
    assert np.isnan(lat_off[0])
    assert np.isnan(lon_off[0])


def test_fill_gaps_propagates_into_nan_holes():
    data = np.full((5, 5), np.nan, dtype=np.float32)
    data[2, 2] = 10.0
    filled = goes.fill_gaps(data, iterations=3)
    assert filled[2, 1] == 10.0
    assert filled[2, 3] == 10.0


def test_get_satellite_bucket_switches_at_cutover():
    assert goes._get_satellite_bucket(datetime.date(2024, 1, 1)) == (16, "noaa-goes16")
    assert goes._get_satellite_bucket(datetime.date(2025, 1, 14)) == (19, "noaa-goes19")


def test_parse_scan_start_extracts_utc_datetime():
    key = "ABI-L2-CMIPF/2024/270/12/OR_ABI-L2-CMIPF-M6C13_G16_s20242701200207_e20242701209527_c20242701209599.nc"
    dt = goes._parse_scan_start(key)
    assert dt == datetime.datetime(2024, 9, 26, 12, 0, 20, tzinfo=datetime.timezone.utc)


# ── ir4 colortable ────────────────────────────────────────────────────────
def test_ir4_matches_cited_breakpoints_exactly():
    # Source: satpy's colorized_ir_clouds enhancement (generic.yaml) + the
    # ColorBrewer Spectral-11 palette it references. These are exact, not
    # approximations, so check them at full float precision (not through
    # the 256-level _t2i index, which would introduce quantization noise).
    K = 273.15
    assert goes._goes_ir4(30 + K) == [0, 0, 0]            # warmest clipped to black
    assert goes._goes_ir4(0 + K) == [153, 153, 153]        # mid greyscale
    assert goes._goes_ir4(-20 + K) == [94, 79, 162]         # grey/color boundary -> Spectral[10] (purple)
    assert goes._goes_ir4(-80 + K) == [158, 1, 66]          # coldest -> Spectral[0] (dark red)
    assert goes._goes_ir4(-90 + K) == [158, 1, 66]          # colder than -80C clipped, not extrapolated


def test_ir4_registered_in_luts():
    assert "ir4" in goes.LUTS
    assert goes.LUTS["ir4"].shape == (256, 3)


# ── bbox request validation ──────────────────────────────────────────────
def test_resolve_bbox_request_defaults_to_native_resolution():
    bbox = goes.resolve_bbox_request(25.5, -80.3, 500.0, None, band=13)
    assert bbox.resolution_km == goes.NATIVE_GSD_KM[13]


def test_resolve_bbox_request_cannot_exceed_native_resolution():
    # Asking for finer than native (e.g. 0.5 km/px) clamps up to native (2km),
    # since the sensor physically can't resolve finer than that.
    bbox = goes.resolve_bbox_request(25.5, -80.3, 500.0, 0.5, band=13)
    assert bbox.resolution_km == goes.NATIVE_GSD_KM[13]


def test_resolve_bbox_request_clamps_width_to_bounds():
    too_small = goes.resolve_bbox_request(0, 0, 1.0, None, band=13)
    assert too_small.width_km == goes.MIN_BBOX_WIDTH_KM
    too_large = goes.resolve_bbox_request(0, 0, 50000.0, None, band=13)
    assert too_large.width_km == goes.MAX_BBOX_WIDTH_KM


def test_resolve_bbox_request_rejects_invalid_lat_lon():
    with pytest.raises(ValueError):
        goes.resolve_bbox_request(95.0, 0.0, 500.0, None, band=13)
    with pytest.raises(ValueError):
        goes.resolve_bbox_request(0.0, 200.0, 500.0, None, band=13)


@pytest.mark.skipif(not NETWORK_TESTS, reason="set NOAA_RECON_API_NETWORK_TESTS=1 to hit real NOAA S3")
def test_resolve_and_render_matches_goes_tile_py_bounds(tmp_path):
    """End-to-end sanity check: render a known recent scan and confirm the
    bounds/LUT pipeline behaves like the original goes_tile.py (same
    +/-81.3 lat extent, +/-81.0 lon extent around the satellite subpoint)."""
    target = datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(hours=6)
    resolved = goes.resolve_nearest(target, band=13)
    nc_path = goes.ensure_downloaded(resolved, tmp_path / "nc")
    out_png = tmp_path / "out.png"
    meta = goes.render_to_png(nc_path, "bd", out_png)

    assert out_png.exists()
    (lat_s, lon_w), (lat_n, lon_e) = meta["bounds"]
    assert lat_n - lat_s == pytest.approx(162.6, abs=0.1)
    assert lon_e - lon_w == pytest.approx(162.0, abs=0.1)


@pytest.mark.skipif(not NETWORK_TESTS, reason="set NOAA_RECON_API_NETWORK_TESTS=1 to hit real NOAA S3")
def test_bbox_render_is_native_resolution_and_correctly_bounded(tmp_path):
    """Render a small box over a known point (Miami) and confirm: the output
    is sized for native ~2km/px resolution (not the full-disk downsample),
    bounds match the requested box, and a real (non-empty) image comes out."""
    from PIL import Image as PILImage

    target = datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(hours=6)
    resolved = goes.resolve_nearest(target, band=13)
    nc_path = goes.ensure_downloaded(resolved, tmp_path / "nc")
    out_png = tmp_path / "bbox.png"

    bbox = goes.resolve_bbox_request(25.7617, -80.1918, 500.0, None, band=13)  # Miami, 500km box
    meta = goes.render_bbox_to_png(nc_path, 13, "ir4", out_png, bbox)

    assert out_png.exists()
    (lat_s, lon_w), (lat_n, lon_e) = meta["bounds"]
    assert lat_n - lat_s == pytest.approx(500.0 / goes.KM_PER_DEG_LAT, rel=0.05)
    assert meta["resolution_km"] == 2.0
    assert meta["out_size"] == pytest.approx(250, abs=2)  # 500km / 2km-per-px

    im = PILImage.open(out_png)
    assert im.size == (meta["out_size"], meta["out_size"])
    arr = np.array(im)
    assert (arr[:, :, 3] > 0).sum() > 0  # at least some opaque (real data) pixels
