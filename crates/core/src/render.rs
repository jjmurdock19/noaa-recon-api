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
