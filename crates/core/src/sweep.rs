//! TDR sweep slicing — pure array math for `GET /v1/tdr/sweep`, no I/O.
//!
//! Two source shapes, both already gridded (no Doppler synthesis happens
//! here — that's HRD's variational analysis software, done long before this
//! data reaches us):
//!
//! - **`xy` volume**: a flattened `(x, y, level, time)` netCDF array. netCDF/
//!   HDF5 always lays out a variable's data in C order matching the
//!   *declared* dimension order, so with dims declared `(x, y, level, time)`
//!   — confirmed against a real 2024 `xy.nc` file, not assumed — `x` is the
//!   slowest-varying (outermost) index and `time` the fastest: flat index =
//!   `((xi*ny + yi)*nlevel + level_idx)*ntime + ti`. [`cappi_slice`] extracts
//!   one horizontal (constant-height) plane.
//! - **`vert_inbound`/`vert_outbound` profile**: a flattened `(radius,
//!   heading, height, time)` array with `heading`/`time` both singleton (one
//!   azimuth, one analysis time) — confirmed against a real file. So the
//!   whole product is already the 2D slice: [`vertical_profile_slice`]
//!   extracts the `(radius, height)` plane.
//!
//! Both return `data[row][col]` in the orientation a Plotly heatmap expects
//! (`z[row][col]` against `x`=columns, `y`=rows) — `cappi_slice` rows on `y`
//! (north-up map), `vertical_profile_slice` rows on `height` (bottom-up
//! cross-section).

/// A value very close to `missing` (or NaN) reads as "no data" — this
/// dataset uses a `missing_value` attribute (commonly `-999.9`), not
/// `_FillValue`, and the same sentinel convention shows up throughout its
/// global attributes (`AZBIEL`, `THRESH`, etc. all default to -999).
fn is_missing(v: f32, missing: f32) -> bool {
    v.is_nan() || (missing.is_finite() && (v - missing).abs() < 0.01)
}

/// Nearest coordinate index to `target` — e.g. picking the `level` (height,
/// km) index closest to a requested CAPPI altitude.
pub fn nearest_index(coords: &[f32], target: f32) -> usize {
    coords
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| (**a - target).abs().partial_cmp(&(**b - target).abs()).unwrap())
        .map(|(i, _)| i)
        .unwrap_or(0)
}

/// One horizontal (x,y) plane at a fixed `level_idx`, from a flattened
/// `(x, y, level, time=1)` array. Returns `data[yi][xi]`.
pub fn cappi_slice(flat: &[f32], nx: usize, ny: usize, nlevel: usize, level_idx: usize, missing: f32) -> Vec<Vec<Option<f32>>> {
    let mut out = vec![vec![None; nx]; ny];
    for xi in 0..nx {
        for yi in 0..ny {
            let idx = (xi * ny + yi) * nlevel + level_idx;
            let v = flat.get(idx).copied().unwrap_or(f32::NAN);
            out[yi][xi] = if is_missing(v, missing) { None } else { Some(v) };
        }
    }
    out
}

/// The full `(level, y, x)` volume from a flattened `(x, y, level, time=1)`
/// array — every CAPPI plane, not just one, for a genuine 3D view.
/// `out[level_idx][yi][xi]`, same missing-value masking as [`cappi_slice`].
pub fn xy_volume(flat: &[f32], nx: usize, ny: usize, nlevel: usize, missing: f32) -> Vec<Vec<Vec<Option<f32>>>> {
    (0..nlevel).map(|level_idx| cappi_slice(flat, nx, ny, nlevel, level_idx, missing)).collect()
}

/// Max-value projection across the `level` axis of a flattened `(x, y,
/// level, time=1)` array — a "composite reflectivity"-style flattening of
/// the whole altitude column into one horizontal plane, ignoring missing
/// values at any given level. Returns `data[yi][xi]`, same orientation as
/// [`cappi_slice`]. `None` only where every level is missing at that (x,y).
pub fn max_projection(flat: &[f32], nx: usize, ny: usize, nlevel: usize, missing: f32) -> Vec<Vec<Option<f32>>> {
    let mut out = vec![vec![None; nx]; ny];
    for xi in 0..nx {
        for yi in 0..ny {
            let mut best: Option<f32> = None;
            for level_idx in 0..nlevel {
                let idx = (xi * ny + yi) * nlevel + level_idx;
                let v = flat.get(idx).copied().unwrap_or(f32::NAN);
                if is_missing(v, missing) {
                    continue;
                }
                best = Some(best.map_or(v, |b| b.max(v)));
            }
            out[yi][xi] = best;
        }
    }
    out
}

/// Pixel-wise max across several already-sliced `(y, x)` planes sharing the
/// same grid — used to mosaic one CAPPI level across a mission's whole
/// timeline (see `GET /v1/tdr/composite?mode=time`). All `planes` must have
/// identical dimensions; the caller is responsible for grid-compatibility
/// (same `x`/`y` coordinate arrays) since this function only sees values.
pub fn max_composite(planes: &[Vec<Vec<Option<f32>>>]) -> Vec<Vec<Option<f32>>> {
    let Some(first) = planes.first() else { return Vec::new() };
    let (ny, nx) = (first.len(), first.first().map_or(0, |r| r.len()));
    let mut out = vec![vec![None; nx]; ny];
    for plane in planes {
        for yi in 0..ny {
            for xi in 0..nx {
                if let Some(v) = plane[yi][xi] {
                    out[yi][xi] = Some(out[yi][xi].map_or(v, |b: f32| b.max(v)));
                }
            }
        }
    }
    out
}

/// The `(radius, height)` plane from a flattened `(radius, heading=1,
/// height, time=1)` array. Returns `data[zi][ri]` (height rows, radius
/// columns) so a Plotly heatmap plots height bottom-up along `y` and
/// along-track radius along `x`, matching the along-track vertical-profile
/// convention the AOML TDR README describes.
pub fn vertical_profile_slice(flat: &[f32], nradius: usize, nheight: usize, missing: f32) -> Vec<Vec<Option<f32>>> {
    let mut out = vec![vec![None; nradius]; nheight];
    for ri in 0..nradius {
        for zi in 0..nheight {
            let idx = ri * nheight + zi;
            let v = flat.get(idx).copied().unwrap_or(f32::NAN);
            out[zi][ri] = if is_missing(v, missing) { None } else { Some(v) };
        }
    }
    out
}

/// Approximate local flat-earth offset (km) of `(lat, lon)` relative to a
/// reference `(lat0, lon0)` — accurate enough for aligning TDR sweeps
/// separated by tens to a couple hundred km (a single mission's track),
/// not meant for anything requiring true geodesic distance.
pub fn latlon_offset_km(lat: f32, lon: f32, lat0: f32, lon0: f32) -> (f32, f32) {
    const KM_PER_DEG_LAT: f32 = 110.574;
    const KM_PER_DEG_LON_AT_EQUATOR: f32 = 111.320;
    let km_per_deg_lon = KM_PER_DEG_LON_AT_EQUATOR * lat0.to_radians().cos();
    let dx = (lon - lon0) * km_per_deg_lon;
    let dy = (lat - lat0) * KM_PER_DEG_LAT;
    (dx, dy)
}

/// One sweep to be placed into a [`geo_mosaic`] — its own grid plus how far
/// (km) that grid's origin sits from the mosaic's shared reference point
/// (see [`latlon_offset_km`]).
pub struct GeoPlane<'a> {
    pub x: &'a [f32],
    pub y: &'a [f32],
    /// `data[yi][xi]`, same orientation as [`cappi_slice`].
    pub data: &'a [Vec<Option<f32>>],
    pub offset_x_km: f32,
    pub offset_y_km: f32,
}

pub struct Mosaic {
    pub x: Vec<f32>,
    pub y: Vec<f32>,
    /// `data[yi][xi]`.
    pub data: Vec<Vec<Option<f32>>>,
}

/// Forward-scatters several storm-centered sweeps, each shifted by its own
/// (lat,lon)-derived offset from a shared reference point, onto one shared
/// output grid — the "align by storm center, build one composite" mosaic
/// backing `GET /v1/tdr/composite?mode=time`. Where two sweeps land on the
/// same output cell, keeps the max value (same "composite reflectivity"
/// convention as [`max_projection`]/[`max_composite`]).
///
/// Assumes every plane shares the same grid spacing (true for TDR's fixed
/// analysis resolution) — spacing is read from the first plane. Returns an
/// empty mosaic if `planes` is empty.
pub fn geo_mosaic(planes: &[GeoPlane]) -> Mosaic {
    let Some(first) = planes.first() else {
        return Mosaic { x: Vec::new(), y: Vec::new(), data: Vec::new() };
    };
    let dx_spacing = if first.x.len() >= 2 { (first.x[1] - first.x[0]).abs() } else { 1.0 };
    let dy_spacing = if first.y.len() >= 2 { (first.y[1] - first.y[0]).abs() } else { 1.0 };

    let (mut gx_min, mut gx_max) = (f32::INFINITY, f32::NEG_INFINITY);
    let (mut gy_min, mut gy_max) = (f32::INFINITY, f32::NEG_INFINITY);
    for p in planes {
        for &x in p.x {
            gx_min = gx_min.min(x + p.offset_x_km);
            gx_max = gx_max.max(x + p.offset_x_km);
        }
        for &y in p.y {
            gy_min = gy_min.min(y + p.offset_y_km);
            gy_max = gy_max.max(y + p.offset_y_km);
        }
    }
    if !gx_min.is_finite() || !gy_min.is_finite() {
        return Mosaic { x: Vec::new(), y: Vec::new(), data: Vec::new() };
    }

    let nx_out = (((gx_max - gx_min) / dx_spacing).round() as usize) + 1;
    let ny_out = (((gy_max - gy_min) / dy_spacing).round() as usize) + 1;
    let x_out: Vec<f32> = (0..nx_out).map(|i| gx_min + i as f32 * dx_spacing).collect();
    let y_out: Vec<f32> = (0..ny_out).map(|i| gy_min + i as f32 * dy_spacing).collect();
    let mut data_out = vec![vec![None; nx_out]; ny_out];

    for p in planes {
        for (yi, y) in p.y.iter().enumerate() {
            for (xi, x) in p.x.iter().enumerate() {
                let Some(v) = p.data[yi][xi] else { continue };
                let shifted_x = x + p.offset_x_km;
                let shifted_y = y + p.offset_y_km;
                let out_xi = ((shifted_x - gx_min) / dx_spacing).round();
                let out_yi = ((shifted_y - gy_min) / dy_spacing).round();
                if out_xi < 0.0 || out_yi < 0.0 {
                    continue;
                }
                let (out_xi, out_yi) = (out_xi as usize, out_yi as usize);
                if out_xi >= nx_out || out_yi >= ny_out {
                    continue;
                }
                data_out[out_yi][out_xi] = Some(data_out[out_yi][out_xi].map_or(v, |b: f32| b.max(v)));
            }
        }
    }

    Mosaic { x: x_out, y: y_out, data: data_out }
}

/// A Plotly-style `[[fraction, hex], ...]` colorscale plus the physical
/// value domain it maps to (`zmin`/`zmax`) — mirrors the same
/// stops-plus-domain shape `/v1/satellite/colortable` already returns, so a
/// client builds a legend the same way for both endpoints.
pub struct FieldColorscale {
    pub stops: Vec<(f32, &'static str)>,
    pub zmin: f32,
    pub zmax: f32,
    pub units: &'static str,
}

/// Reflectivity (dBZ, 0-70) uses the common green→yellow→red→magenta radar
/// convention; every other field this endpoint serves (Doppler-derived
/// wind: radial/tangential/u/v/w/vorticity/speed) is a physical velocity
/// that can be negative, so it gets a blue-white-red diverging scale
/// instead — a sequential-only scale would visually erase the sign, which
/// is the physically meaningful part (inbound vs outbound flow, updraft vs
/// downdraft). `wind_speed` (magnitude, always >= 0) is the one exception
/// and gets its own sequential scale.
pub fn colorscale_for_field(field: &str) -> FieldColorscale {
    match field {
        "reflectivity" => FieldColorscale {
            stops: vec![
                (0.0, "#04e9e7"),
                (0.21, "#019000"),
                (0.43, "#fdf802"),
                (0.57, "#ff9000"),
                (0.71, "#ff0000"),
                (0.86, "#ff00ff"),
                (1.0, "#ffffff"),
            ],
            zmin: 0.0,
            zmax: 70.0,
            units: "dBZ",
        },
        "wind_speed" => FieldColorscale {
            stops: vec![
                (0.0, "#08306b"),
                (0.25, "#2171b5"),
                (0.5, "#41ab5d"),
                (0.75, "#fdae61"),
                (1.0, "#a50f15"),
            ],
            zmin: 0.0,
            zmax: 80.0,
            units: "m/s",
        },
        _ => FieldColorscale {
            stops: vec![
                (0.0, "#2166ac"),
                (0.5, "#f7f7f7"),
                (1.0, "#b2182b"),
            ],
            zmin: -40.0,
            zmax: 40.0,
            units: "m/s",
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cappi_slice_picks_the_right_level_and_orientation() {
        // nx=2, ny=2, nlevel=2. flat index = (xi*ny+yi)*nlevel + level.
        // (x=0,y=0)->[0,10] (x=0,y=1)->[1,11] (x=1,y=0)->[2,12] (x=1,y=1)->[3,13]
        let flat = vec![0.0, 10.0, 1.0, 11.0, 2.0, 12.0, 3.0, 13.0];
        let level0 = cappi_slice(&flat, 2, 2, 2, 0, -999.9);
        assert_eq!(level0, vec![vec![Some(0.0), Some(2.0)], vec![Some(1.0), Some(3.0)]]);
        let level1 = cappi_slice(&flat, 2, 2, 2, 1, -999.9);
        assert_eq!(level1, vec![vec![Some(10.0), Some(12.0)], vec![Some(11.0), Some(13.0)]]);
    }

    #[test]
    fn cappi_slice_masks_missing_value() {
        let flat = vec![-999.9, 5.0, 5.0, 5.0];
        let slice = cappi_slice(&flat, 2, 2, 1, 0, -999.9);
        assert_eq!(slice[0][0], None);
        assert_eq!(slice[1][0], Some(5.0));
    }

    #[test]
    fn vertical_profile_slice_orients_height_as_rows() {
        // nradius=2, nheight=3. flat index = ri*nheight + zi.
        let flat = vec![0.0, 1.0, 2.0, 10.0, 11.0, 12.0];
        let out = vertical_profile_slice(&flat, 2, 3, -999.9);
        // out[zi][ri]
        assert_eq!(out[0], vec![Some(0.0), Some(10.0)]);
        assert_eq!(out[1], vec![Some(1.0), Some(11.0)]);
        assert_eq!(out[2], vec![Some(2.0), Some(12.0)]);
    }

    #[test]
    fn xy_volume_returns_every_level_in_cappi_orientation() {
        let flat = vec![0.0, 10.0, 1.0, 11.0, 2.0, 12.0, 3.0, 13.0];
        let vol = xy_volume(&flat, 2, 2, 2, -999.9);
        assert_eq!(vol.len(), 2);
        assert_eq!(vol[0], cappi_slice(&flat, 2, 2, 2, 0, -999.9));
        assert_eq!(vol[1], cappi_slice(&flat, 2, 2, 2, 1, -999.9));
    }

    #[test]
    fn max_projection_takes_max_across_levels_and_skips_missing() {
        // nx=1, ny=1, nlevel=3: values 5, -999.9 (missing), 8 at that pixel.
        let flat = vec![5.0, -999.9, 8.0];
        let out = max_projection(&flat, 1, 1, 3, -999.9);
        assert_eq!(out[0][0], Some(8.0));

        // Every level missing -> None.
        let all_missing = vec![-999.9, -999.9];
        let out = max_projection(&all_missing, 1, 1, 2, -999.9);
        assert_eq!(out[0][0], None);
    }

    #[test]
    fn max_composite_merges_planes_pixelwise() {
        let a = vec![vec![Some(1.0), None], vec![Some(3.0), Some(4.0)]];
        let b = vec![vec![Some(2.0), Some(5.0)], vec![None, Some(1.0)]];
        let out = max_composite(&[a, b]);
        assert_eq!(out, vec![vec![Some(2.0), Some(5.0)], vec![Some(3.0), Some(4.0)]]);
    }

    #[test]
    fn latlon_offset_km_is_zero_at_reference_and_scales_with_distance() {
        let (dx, dy) = latlon_offset_km(25.0, -80.0, 25.0, -80.0);
        assert!((dx).abs() < 1e-4 && (dy).abs() < 1e-4);

        // 1 degree of latitude north -> ~110.6 km north (positive dy).
        let (dx, dy) = latlon_offset_km(26.0, -80.0, 25.0, -80.0);
        assert!((dy - 110.574).abs() < 0.01);
        assert!(dx.abs() < 1e-4);
    }

    #[test]
    fn geo_mosaic_aligns_and_max_composites_shifted_planes() {
        // Two 1x1 planes on a 1km grid, second shifted +1km in x.
        let x = [0.0f32, 1.0];
        let y = [0.0f32];
        let a = vec![vec![Some(3.0), None]];
        let b = vec![vec![Some(5.0), Some(9.0)]];
        let planes = vec![
            GeoPlane { x: &x, y: &y, data: &a, offset_x_km: 0.0, offset_y_km: 0.0 },
            GeoPlane { x: &x, y: &y, data: &b, offset_x_km: 1.0, offset_y_km: 0.0 },
        ];
        let mosaic = geo_mosaic(&planes);
        // Combined x extent: plane a covers [0,1], plane b (shifted) covers [1,2] -> [0,1,2].
        assert_eq!(mosaic.x, vec![0.0, 1.0, 2.0]);
        assert_eq!(mosaic.y, vec![0.0]);
        // col0 (x=0): only a's x=0 -> 3.0. col1 (x=1): a's x=1 (missing) + b's x=0 (5.0) -> 5.0.
        // col2 (x=2): only b's x=1 -> 9.0.
        assert_eq!(mosaic.data[0], vec![Some(3.0), Some(5.0), Some(9.0)]);
    }

    #[test]
    fn geo_mosaic_empty_input_returns_empty() {
        let mosaic = geo_mosaic(&[]);
        assert!(mosaic.x.is_empty() && mosaic.y.is_empty() && mosaic.data.is_empty());
    }

    #[test]
    fn nearest_index_picks_closest() {
        let levels = [0.0, 0.5, 1.0, 1.5, 2.0];
        assert_eq!(nearest_index(&levels, 1.6), 3);
        assert_eq!(nearest_index(&levels, -5.0), 0);
        assert_eq!(nearest_index(&levels, 50.0), 4);
    }
}
