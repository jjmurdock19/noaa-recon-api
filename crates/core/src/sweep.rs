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
    fn nearest_index_picks_closest() {
        let levels = [0.0, 0.5, 1.0, 1.5, 2.0];
        assert_eq!(nearest_index(&levels, 1.6), 3);
        assert_eq!(nearest_index(&levels, -5.0), 0);
        assert_eq!(nearest_index(&levels, 50.0), 4);
    }
}
