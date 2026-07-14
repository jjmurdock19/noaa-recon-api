//! GOES render pipeline — port of the numpy render half of
//! `app/services/goes.py` (paint/fill/smooth/colorize + the full-disk and bbox
//! drivers). Pure `Vec<f32>` math, WASM-safe: takes an already-decoded CMI grid
//! (the netCDF read stays in the server) and returns RGBA bytes + geo bounds.

use crate::bbox::bbox_bounds;
use crate::catalog::is_reflectance_band;
use crate::colormap::{build_lut, reflectance_gray, stops_by_cmap, t2i, Rgb};
use crate::project::{abi_to_latlon, mercator_y};

/// Result of a render: `rgba` is `out_size*out_size*4` bytes, row-major.
pub struct RenderResult {
    pub rgba: Vec<u8>,
    pub out_size: usize,
    pub bounds: [[f64; 2]; 2],
    pub sat_lon: f64,
}

#[derive(Clone, Copy)]
pub struct Proj {
    pub sat_lon: f64,
    pub h: f64,
    pub r_eq: f64,
    pub r_pol: f64,
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

/// Forward-scatter source pixels onto the output canvas keeping the COLDEST
/// (minimum) value on collisions (`_paint_coldest` + the projection loop shared
/// by `_project_to_canvas`/`_project_crop_to_canvas`). Returns `None` if nothing
/// landed on-canvas.
#[allow(clippy::too_many_arguments)]
fn paint_project(
    x: &[f64],
    y: &[f64],
    cmi: &[f32],
    ny: usize,
    nx: usize,
    p: Proj,
    lon_w: f64,
    lon_e: f64,
    merc_y_s: f64,
    merc_y_n: f64,
    out_size: usize,
) -> Option<Vec<f32>> {
    let mut out = vec![f32::INFINITY; out_size * out_size];
    let mut any = false;
    let osz = out_size as f64;
    for i in 0..ny {
        let yr = y[i];
        for j in 0..nx {
            let v = cmi[i * nx + j];
            if !v.is_finite() {
                continue;
            }
            let (lon, lat) = match abi_to_latlon(x[j], yr, p.sat_lon, p.h, p.r_eq, p.r_pol) {
                Some(ll) => ll,
                None => continue,
            };
            // np.astype(int32) truncates toward zero; `as i64` matches.
            let col = ((lon - lon_w) / (lon_e - lon_w) * osz) as i64;
            let row = ((merc_y_n - mercator_y(lat)) / (merc_y_n - merc_y_s) * osz) as i64;
            if col >= 0 && col < out_size as i64 && row >= 0 && row < out_size as i64 {
                let idx = row as usize * out_size + col as usize;
                if v < out[idx] {
                    out[idx] = v;
                    any = true;
                }
            }
        }
    }
    if !any {
        return None;
    }
    for cell in out.iter_mut() {
        if cell.is_infinite() {
            *cell = f32::NAN;
        }
    }
    Some(out)
}

/// `fill_gaps`: nearest-neighbor NaN fill, 4-directional, `iterations` passes,
/// with `np.roll` wraparound at the edges.
fn fill_gaps(data: &mut [f32], n: usize, iterations: usize) {
    let ni = n as i64;
    for _ in 0..iterations {
        for (dy, dx) in [(-1i64, 0i64), (1, 0), (0, -1), (0, 1)] {
            let snap = data.to_vec();
            for i in 0..n {
                for j in 0..n {
                    let idx = i * n + j;
                    if snap[idx].is_nan() {
                        let si = ((i as i64 - dy).rem_euclid(ni)) as usize;
                        let sj = ((j as i64 - dx).rem_euclid(ni)) as usize;
                        let s = snap[si * n + sj];
                        if s.is_finite() {
                            data[idx] = s;
                        }
                    }
                }
            }
        }
    }
}

/// `_smooth`: NaN-aware 3x3 box blur, `passes` passes, only updating valid
/// cells, `np.roll` wraparound.
fn smooth(data: &mut [f32], n: usize, passes: usize) {
    let ni = n as i64;
    for _ in 0..passes {
        let snap = data.to_vec();
        let mut vsum = vec![0f32; n * n];
        let mut wsum = vec![0f32; n * n];
        for dy in -1i64..=1 {
            for dx in -1i64..=1 {
                for i in 0..n {
                    for j in 0..n {
                        let si = ((i as i64 - dy).rem_euclid(ni)) as usize;
                        let sj = ((j as i64 - dx).rem_euclid(ni)) as usize;
                        let sidx = si * n + sj;
                        let idx = i * n + j;
                        if snap[sidx].is_finite() {
                            vsum[idx] += snap[sidx];
                            wsum[idx] += 1.0;
                        }
                    }
                }
            }
        }
        for idx in 0..n * n {
            if snap[idx].is_finite() {
                data[idx] = vsum[idx] / wsum[idx];
            }
        }
    }
}

/// `_colorize`: map the (NaN-holed) value grid to RGBA. `band` selects the
/// reflectance vs brightness-temperature path; `cmap` selects the enhancement.
fn colorize(output: &[f32], n: usize, cmap: &str, band: i64) -> Vec<u8> {
    let mut rgba = vec![0u8; n * n * 4];
    let reflectance = is_reflectance_band(band);
    let stops = stops_by_cmap(cmap);
    let lut: Option<[Rgb; 256]> = if reflectance || stops.is_some() {
        None
    } else {
        Some(build_lut(cmap).unwrap_or_else(|| build_lut("bd").unwrap()))
    };

    for idx in 0..n * n {
        let v = output[idx];
        if !v.is_finite() {
            continue; // alpha stays 0
        }
        let rgb: Rgb = if reflectance {
            let g = reflectance_gray(v as f64, 1.5);
            [g, g, g]
        } else if let Some(stops) = stops {
            crate::colormap::interp_stops_public(v as f64 - 273.15, stops)
        } else {
            lut.unwrap()[t2i(v as f64) as usize]
        };
        let o = idx * 4;
        rgba[o] = rgb[0];
        rgba[o + 1] = rgb[1];
        rgba[o + 2] = rgb[2];
        rgba[o + 3] = 220;
    }
    rgba
}

/// Full-disk single-band render (`render_to_png`, minus the file I/O). `cmi` is
/// the already-downsampled CMI grid (`ny`x`nx`), `x`/`y` its fixed-grid coords.
#[allow(clippy::too_many_arguments)]
pub fn render_full_disk(
    cmi: &[f32],
    ny: usize,
    nx: usize,
    x: &[f64],
    y: &[f64],
    p: Proj,
    cmap: &str,
    band: i64,
    out_size: usize,
) -> RenderResult {
    let (lat_s, lat_n) = (-81.3, 81.3);
    let (lon_w, lon_e) = (p.sat_lon - 81.0, p.sat_lon + 81.0);
    let (msy, mny) = (mercator_y(lat_s), mercator_y(lat_n));

    let mut out = paint_project(x, y, cmi, ny, nx, p, lon_w, lon_e, msy, mny, out_size)
        .unwrap_or_else(|| vec![f32::NAN; out_size * out_size]);
    fill_gaps(&mut out, out_size, 6);
    if stops_by_cmap(cmap).is_none() {
        smooth(&mut out, out_size, 1);
    }
    let rgba = colorize(&out, out_size, cmap, band);
    RenderResult {
        rgba,
        out_size,
        bounds: [[lat_s, lon_w], [lat_n, lon_e]],
        sat_lon: round1(p.sat_lon),
    }
}

/// Bounding-box single-band render (`render_bbox_to_png`, minus file I/O and the
/// netCDF crop read). Coordinates/values are the already-cropped native-res
/// arrays; `center_lat/lon`/`width_km`/`out_size` come from the resolved bbox.
#[allow(clippy::too_many_arguments)]
pub fn render_bbox(
    cmi_crop: &[f32],
    ny: usize,
    nx: usize,
    x_crop: &[f64],
    y_crop: &[f64],
    p: Proj,
    cmap: &str,
    band: i64,
    center_lat: f64,
    center_lon: f64,
    width_km: f64,
    out_size: usize,
) -> Result<RenderResult, String> {
    let (lat_s, lat_n, lon_w, lon_e) = bbox_bounds(center_lat, center_lon, width_km);
    let (msy, mny) = (mercator_y(lat_s), mercator_y(lat_n));

    let mut out = paint_project(x_crop, y_crop, cmi_crop, ny, nx, p, lon_w, lon_e, msy, mny, out_size)
        .ok_or_else(|| "Requested area has no valid data in this scan (off-disk or no-data)".to_string())?;
    fill_gaps(&mut out, out_size, 6);
    if stops_by_cmap(cmap).is_none() {
        smooth(&mut out, out_size, 1);
    }
    let rgba = colorize(&out, out_size, cmap, band);
    Ok(RenderResult {
        rgba,
        out_size,
        bounds: [[lat_s, lon_w], [lat_n, lon_e]],
        sat_lon: round1(p.sat_lon),
    })
}

// ── Composite products (sandwich / geocolor) ────────────────────────────────
// Port of the composite half of `app/services/goes.py`. Each composite's
// server-side caller (`services/goes.rs`) decodes every companion band it
// needs and reprojects each onto one shared canvas via
// `project_band_to_canvas` below (the multi-band counterpart of
// `render_full_disk`/`render_bbox`'s single-band `paint_project` +
// `fill_gaps` — same shared out_size x out_size canvas bounds for every
// band, since they come from the same satellite/projection); the functions
// here take those already-reprojected canvases and do the pure-math
// composition/coloring.

/// Reproject one band's already-decoded (and, for full-disk, already
/// downsampled) CMI onto a shared canvas: `paint_project` + `fill_gaps`, the
/// same two steps `render_full_disk`/`render_bbox` run inline for a single
/// band. `None` means nothing from this band landed inside the canvas
/// bounds at all (composite callers decide whether that's fatal — see
/// `render_sandwich`/`render_geocolor`).
#[allow(clippy::too_many_arguments)]
pub fn project_band_to_canvas(
    cmi: &[f32],
    ny: usize,
    nx: usize,
    x: &[f64],
    y: &[f64],
    p: Proj,
    lon_w: f64,
    lon_e: f64,
    merc_y_s: f64,
    merc_y_n: f64,
    out_size: usize,
) -> Option<Vec<f32>> {
    let mut out = paint_project(x, y, cmi, ny, nx, p, lon_w, lon_e, merc_y_s, merc_y_n, out_size)?;
    fill_gaps(&mut out, out_size, 6);
    Some(out)
}

/// Low-precision solar zenith angle in degrees (NOAA/Spencer Fourier-series
/// approximation — public-domain, the same formula behind NOAA's online
/// solar calculator; `_solar_zenith_deg` in Python). Accurate to a fraction
/// of a degree, plenty for a several-degree-wide terminator blend; not
/// ephemeris-grade. `doy` is UTC day-of-year (1-366); `hour`/`minute`/
/// `second` are the UTC time-of-day (kept as separate components, rather
/// than taking a `DateTime`, to keep this crate WASM-safe / chrono-free).
pub fn solar_zenith_deg(lat_deg: f64, lon_deg: f64, doy: i64, hour: u32, minute: u32, second: f64) -> f64 {
    use std::f64::consts::PI;
    let fy = 2.0 * PI / 365.0 * ((doy - 1) as f64 + (hour as f64 - 12.0) / 24.0);
    let decl = 0.006918 - 0.399912 * fy.cos() + 0.070257 * fy.sin() - 0.006758 * (2.0 * fy).cos()
        + 0.000907 * (2.0 * fy).sin()
        - 0.002697 * (3.0 * fy).cos()
        + 0.00148 * (3.0 * fy).sin();
    let eqtime = 229.18
        * (0.000075 + 0.001868 * fy.cos() - 0.032077 * fy.sin() - 0.014615 * (2.0 * fy).cos()
            - 0.040849 * (2.0 * fy).sin());
    let time_offset = eqtime + 4.0 * lon_deg;
    let true_solar_time = hour as f64 * 60.0 + minute as f64 + second / 60.0 + time_offset;
    let hour_angle = (true_solar_time / 4.0 - 180.0).to_radians();
    let lat_rad = lat_deg.to_radians();
    let cos_zenith = lat_rad.sin() * decl.sin() + lat_rad.cos() * decl.cos() * hour_angle.cos();
    cos_zenith.clamp(-1.0, 1.0).acos().to_degrees()
}

/// IR/VIS "sandwich" composite: the standard abi13 colorized IR, modulated
/// (multiplied) by Band 2 visible brightness — surfaces convective texture
/// (overshooting tops, gravity waves, low cloud streets) a pure IR
/// colorization smooths over. `ir_canvas`/`vis_canvas` are both already
/// projected onto the same shared canvas (`project_band_to_canvas`).
/// `vis_canvas: None` (VIS band unavailable for this crop entirely) and
/// individual NaN pixels within `Some` (night side, or no data) both fall
/// back to a darkened — not hidden — version of the plain IR colorization,
/// matching the Python's `_apply_stops_exact` + `nan_to_num` behavior.
pub fn render_sandwich(
    ir_canvas: &[f32],
    vis_canvas: Option<&[f32]>,
    out_size: usize,
    bounds: [[f64; 2]; 2],
    sat_lon: f64,
) -> RenderResult {
    let stops = stops_by_cmap("abi13").expect("abi13 stops table");
    let mut rgba = vec![0u8; out_size * out_size * 4];
    for idx in 0..out_size * out_size {
        let k = ir_canvas[idx];
        if !k.is_finite() {
            continue; // alpha stays 0
        }
        let ir_rgb = crate::colormap::interp_stops_public(k as f64 - 273.15, stops);
        let brightness = match vis_canvas.map(|v| v[idx]) {
            Some(r) if r.is_finite() => 0.35 + 0.65 * (r as f64).clamp(0.0, 1.0),
            _ => 0.35,
        };
        let o = idx * 4;
        rgba[o] = (ir_rgb[0] as f64 * brightness).clamp(0.0, 255.0).round() as u8;
        rgba[o + 1] = (ir_rgb[1] as f64 * brightness).clamp(0.0, 255.0).round() as u8;
        rgba[o + 2] = (ir_rgb[2] as f64 * brightness).clamp(0.0, 255.0).round() as u8;
        rgba[o + 3] = 220;
    }
    RenderResult { rgba, out_size, bounds, sat_lon: round1(sat_lon) }
}

/// A simplified GeoColor-*style* day/night composite (NOT NOAA/CIRA's
/// proprietary algorithm — no city-lights layer, no atmospheric/Rayleigh
/// correction). Day side: synthetic true color from Bands 1(blue)/2(red)/3
/// (veggie/NIR) reflectance, with green synthesized via CIRA's published
/// recipe (`green = 0.45*red + 0.10*NIR + 0.45*blue`). Night side: the
/// standard abi13 colorized IR. Blended per-pixel by solar zenith angle —
/// full day color inside ~85 deg zenith, full night IR beyond ~95 deg,
/// linear blend across the ~10 deg terminator band between. `blue`/`red`/
/// `veggie` all `None` together (bbox crop missing all three, or full-disk
/// where they're not requested) forces night-side IR everywhere, matching
/// the Python's `have_day_color` fallback (a partial/wrong day color would
/// be worse than a consistent fallback).
#[allow(clippy::too_many_arguments)]
pub fn render_geocolor(
    ir_canvas: &[f32],
    blue: Option<&[f32]>,
    red: Option<&[f32]>,
    veggie: Option<&[f32]>,
    out_size: usize,
    bounds: [[f64; 2]; 2],
    lon_w: f64,
    lon_e: f64,
    merc_y_s: f64,
    merc_y_n: f64,
    sat_lon: f64,
    doy: i64,
    hour: u32,
    minute: u32,
    second: f64,
) -> RenderResult {
    let have_day_color = blue.is_some() && red.is_some() && veggie.is_some();
    let stops = stops_by_cmap("abi13").expect("abi13 stops table");
    let mut rgba = vec![0u8; out_size * out_size * 4];
    let osz = out_size as f64;
    for row in 0..out_size {
        let merc_y = merc_y_n - (row as f64 + 0.5) / osz * (merc_y_n - merc_y_s);
        let lat = (2.0 * merc_y.exp().atan() - std::f64::consts::FRAC_PI_2).to_degrees();
        for col in 0..out_size {
            let idx = row * out_size + col;
            let k = ir_canvas[idx];
            if !k.is_finite() {
                continue; // alpha stays 0
            }
            let night_rgb = crate::colormap::interp_stops_public(k as f64 - 273.15, stops);

            let (day_r, day_g, day_b, day_weight) = if have_day_color {
                let lon = lon_w + (col as f64 + 0.5) / osz * (lon_e - lon_w);
                let r = red.unwrap()[idx];
                let r = if r.is_finite() { r as f64 } else { 0.0 };
                let g_in = veggie.unwrap()[idx];
                let g_in = if g_in.is_finite() { g_in as f64 } else { 0.0 };
                let b = blue.unwrap()[idx];
                let b = if b.is_finite() { b as f64 } else { 0.0 };
                let green = 0.45 * r + 0.10 * g_in + 0.45 * b;
                let dr = reflectance_gray(r, 1.5) as f64;
                let dg = reflectance_gray(green, 1.5) as f64;
                let db = reflectance_gray(b, 1.5) as f64;
                let zenith = solar_zenith_deg(lat, lon, doy, hour, minute, second);
                let w = ((95.0 - zenith) / (95.0 - 85.0)).clamp(0.0, 1.0);
                (dr, dg, db, w)
            } else {
                (0.0, 0.0, 0.0, 0.0)
            };

            let br = day_weight * day_r + (1.0 - day_weight) * night_rgb[0] as f64;
            let bg = day_weight * day_g + (1.0 - day_weight) * night_rgb[1] as f64;
            let bb = day_weight * day_b + (1.0 - day_weight) * night_rgb[2] as f64;

            let o = idx * 4;
            rgba[o] = br.clamp(0.0, 255.0).round() as u8;
            rgba[o + 1] = bg.clamp(0.0, 255.0).round() as u8;
            rgba[o + 2] = bb.clamp(0.0, 255.0).round() as u8;
            rgba[o + 3] = 220;
        }
    }
    RenderResult { rgba, out_size, bounds, sat_lon: round1(sat_lon) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_gaps_fills_a_hole() {
        // 3x3 with a NaN center surrounded by 5.0; one pass fills it.
        let mut g = vec![5.0f32; 9];
        g[4] = f32::NAN;
        fill_gaps(&mut g, 3, 1);
        assert_eq!(g[4], 5.0);
    }

    #[test]
    fn smooth_averages_neighbors() {
        // uniform field is unchanged by the box blur.
        let mut g = vec![10.0f32; 25];
        smooth(&mut g, 5, 1);
        for v in g {
            assert!((v - 10.0).abs() < 1e-4);
        }
    }

    #[test]
    fn colorize_marks_nan_transparent() {
        let mut g = vec![250.0f32; 4];
        g[0] = f32::NAN;
        let rgba = colorize(&g, 2, "abi13", 13);
        assert_eq!(rgba[3], 0); // NaN -> alpha 0
        assert_eq!(rgba[7], 220); // finite -> alpha 220
    }
}
