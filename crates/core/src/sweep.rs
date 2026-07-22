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

/// Bilinearly samples one CAPPI plane (`data[yi][xi]`, `xs`/`ys` ascending
/// coordinate arrays) at an arbitrary point `(x, y)` — the building block
/// for [`plane_slice`]'s cross-sections, which need values *between* grid
/// columns, not just at them. `None` outside the grid's bounding box.
/// Missing corners don't poison the sample: this renormalizes the bilinear
/// weights over whichever of the 4 surrounding corners are actually
/// present, so e.g. a point next to one masked-out corner still gets a
/// sensible interpolated value instead of silently going missing.
fn bilinear_sample(data: &[Vec<Option<f32>>], xs: &[f32], ys: &[f32], x: f32, y: f32) -> Option<f32> {
    if xs.len() < 2 || ys.len() < 2 {
        return None;
    }
    if x < xs[0] || x > xs[xs.len() - 1] || y < ys[0] || y > ys[ys.len() - 1] {
        return None;
    }
    // Index of the grid cell containing (x, y): last coordinate <= x/y.
    let xi = xs.partition_point(|&v| v <= x).saturating_sub(1).min(xs.len() - 2);
    let yi = ys.partition_point(|&v| v <= y).saturating_sub(1).min(ys.len() - 2);
    let (x0, x1) = (xs[xi], xs[xi + 1]);
    let (y0, y1) = (ys[yi], ys[yi + 1]);
    let tx = if x1 > x0 { (x - x0) / (x1 - x0) } else { 0.0 };
    let ty = if y1 > y0 { (y - y0) / (y1 - y0) } else { 0.0 };

    let corners = [
        (data[yi][xi], (1.0 - tx) * (1.0 - ty)),
        (data[yi][xi + 1], tx * (1.0 - ty)),
        (data[yi + 1][xi], (1.0 - tx) * ty),
        (data[yi + 1][xi + 1], tx * ty),
    ];
    let mut sum = 0.0;
    let mut weight = 0.0;
    for (v, w) in corners {
        if let Some(v) = v {
            sum += v * w;
            weight += w;
        }
    }
    (weight > 0.0).then(|| sum / weight)
}

/// One vertical cross-section cut through an `xy`-volume along an arbitrary
/// line — the "plane slice" tool: pick any two points on the CAPPI image,
/// get the along-track height profile between them, not just the fixed
/// along/across-track cuts baked into `vert_inbound`/`vert_outbound`.
pub struct PlaneSlice {
    /// Distance (km) along the cut line, from `(x0,y0)` to `(x1,y1)`.
    pub along_km: Vec<f32>,
    pub levels: Vec<f32>,
    /// `data[level_idx][along_idx]`, same row-is-height convention as
    /// [`vertical_profile_slice`].
    pub data: Vec<Vec<Option<f32>>>,
}

/// Cuts a [`PlaneSlice`] out of an `xy`-volume (`volume[level_idx][yi][xi]`,
/// as produced by [`xy_volume`]) between `(x0,y0)` and `(x1,y1)` (km,
/// same coordinate system as `xs`/`ys`), sampled at `n_samples` evenly-
/// spaced points along the line (clamped to at least 2). Each sample point
/// is bilinearly interpolated ([`bilinear_sample`]) rather than snapped to
/// the nearest grid column, so the cross-section is smooth regardless of
/// the line's angle through the grid.
pub fn plane_slice(
    volume: &[Vec<Vec<Option<f32>>>],
    xs: &[f32],
    ys: &[f32],
    levels: &[f32],
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    n_samples: usize,
) -> PlaneSlice {
    let n = n_samples.max(2);
    let (dx, dy) = (x1 - x0, y1 - y0);
    let total_km = (dx * dx + dy * dy).sqrt();
    let along_km: Vec<f32> = (0..n).map(|i| total_km * (i as f32 / (n - 1) as f32)).collect();

    let data: Vec<Vec<Option<f32>>> = volume
        .iter()
        .map(|plane| {
            (0..n)
                .map(|i| {
                    let t = i as f32 / (n - 1) as f32;
                    bilinear_sample(plane, xs, ys, x0 + dx * t, y0 + dy * t)
                })
                .collect()
        })
        .collect();

    PlaneSlice { along_km, levels: levels.to_vec(), data }
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

/// How [`geo_mosaic`] resolves two sweeps landing on the same output cell.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CombineMode {
    /// Keep the largest value seen at that cell — the classic "composite
    /// reflectivity" convention (same as [`max_projection`]/
    /// [`max_composite`]): what you'd see if the strongest return from any
    /// analysis time paints through. Right for reflectivity, wrong for
    /// anything where "biggest" isn't "most representative" — see
    /// [`combine_mode_for_field`].
    Max,
    /// Average every value seen at that cell. For instantaneous physical
    /// quantities (wind components, vorticity, speed) sampled at different
    /// times, the mean at an overlapping cell is a far more honest summary
    /// than whichever single analysis time happened to have the largest
    /// reading there — `Max` would systematically bias a wind composite
    /// toward transient gusts instead of the storm's steadier structure.
    Mean,
}

/// The physically-sensible default [`CombineMode`] for a given `xy` field —
/// `Max` for reflectivity (the standard "composite reflectivity" product
/// every radar display uses), `Mean` for everything else (wind components,
/// vorticity, speed — all instantaneous quantities where averaging across
/// overlapping analysis times is more representative than taking an
/// extreme).
pub fn combine_mode_for_field(field: &str) -> CombineMode {
    match field {
        "reflectivity" => CombineMode::Max,
        _ => CombineMode::Mean,
    }
}

/// Forward-scatters several storm-centered sweeps, each shifted by its own
/// (lat,lon)-derived offset from a shared reference point, onto one shared
/// output grid — the "align by storm center, build one composite" mosaic
/// backing `GET /v1/tdr/composite?mode=time`. Where two sweeps land on the
/// same output cell, `mode` decides how their values combine (see
/// [`CombineMode`]) — this is genuine per-cell combination, not last-sweep-
/// wins layering: every plane that touches a cell contributes to it.
///
/// Assumes every plane shares the same grid spacing (true for TDR's fixed
/// analysis resolution) — spacing is read from the first plane. Returns an
/// empty mosaic if `planes` is empty.
pub fn geo_mosaic(planes: &[GeoPlane], mode: CombineMode) -> Mosaic {
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

    // Mean needs a running sum+count per cell alongside the max-so-far;
    // cheap to track both and pick the one `mode` asked for at the end.
    let mut sum_out = vec![vec![0.0f32; nx_out]; ny_out];
    let mut count_out = vec![vec![0u32; nx_out]; ny_out];
    let mut max_out: Vec<Vec<Option<f32>>> = vec![vec![None; nx_out]; ny_out];

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
                sum_out[out_yi][out_xi] += v;
                count_out[out_yi][out_xi] += 1;
                max_out[out_yi][out_xi] = Some(max_out[out_yi][out_xi].map_or(v, |b: f32| b.max(v)));
            }
        }
    }

    let data_out = match mode {
        CombineMode::Max => max_out,
        CombineMode::Mean => (0..ny_out)
            .map(|yi| {
                (0..nx_out)
                    .map(|xi| (count_out[yi][xi] > 0).then(|| sum_out[yi][xi] / count_out[yi][xi] as f32))
                    .collect()
            })
            .collect(),
    };

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

/// Inverse of [`latlon_offset_km`]: turn a local km offset (`dx` east, `dy`
/// north) back into a `(lat, lon)` given the same reference `(lat0, lon0)`.
/// Uses `lat0` for the longitude scaling exactly as [`latlon_offset_km`] does,
/// so the two round-trip. Same flat-earth accuracy caveat.
pub fn latlon_from_offset_km(dx: f32, dy: f32, lat0: f32, lon0: f32) -> (f32, f32) {
    const KM_PER_DEG_LAT: f32 = 110.574;
    const KM_PER_DEG_LON_AT_EQUATOR: f32 = 111.320;
    let lat = lat0 + dy / KM_PER_DEG_LAT;
    let km_per_deg_lon = KM_PER_DEG_LON_AT_EQUATOR * lat0.to_radians().cos();
    let lon = if km_per_deg_lon.abs() > 1e-6 { lon0 + dx / km_per_deg_lon } else { lon0 };
    (lat, lon)
}

// ── TDR-derived storm center (per altitude) ─────────────────────────────────
//
// The gridded xy synthesis stores a *single* storm center for the whole volume
// (the `ORIGIN_LATITUDE/LONGITUDE` global attrs the grid was built around), not
// one per height — but a real vortex tilts with altitude, so its center at each
// CAPPI level differs. We derive that per-level center from the analysis wind
// field (`U`/`V`), using HRD's center criterion: the storm center at a level is
// the point that maximizes the azimuthal-mean *tangential* wind there — i.e.
// the most symmetric cyclonic circulation. An off-center guess mixes radial
// flow into the tangential average and lowers it, so the maximizing point is
// the circulation center. Pure array math, no I/O — the server reads the two
// wind volumes and calls in here; the browser build can too.

/// One CAPPI level's derived storm center. `x_km`/`y_km` are the offset (km
/// east / km north) from the grid origin; the caller maps them to lat/lon with
/// [`latlon_from_offset_km`] and the file's `ORIGIN_LATITUDE/LONGITUDE`. All
/// `Option`s are `None` together when no coherent circulation was found at this
/// level (too little data, or tangential wind below `min_vtan_ms`).
pub struct LevelCenter {
    pub level_km: f32,
    pub x_km: Option<f32>,
    pub y_km: Option<f32>,
    /// Azimuthal-mean tangential wind (m/s) at the found center over the search
    /// annulus — the quantity the search maximized. Diagnostic.
    pub vtan_ms: Option<f32>,
    /// Radius (km) of peak azimuthal-mean tangential wind about the found
    /// center — the radius of maximum wind at this level.
    pub rmw_km: Option<f32>,
}

/// Tunables for [`tangential_wind_centers`]. [`CenterParams::default`] gives
/// sensible values for a typical TC TDR synthesis (annulus 2–50 km, storm
/// within 50 km of the grid origin).
#[derive(Clone, Copy, Debug)]
pub struct CenterParams {
    /// Inner/outer radius (km) of the annulus the azimuthal-mean tangential
    /// wind is evaluated over. Should bracket the radius of maximum wind.
    pub rmin_km: f32,
    pub rmax_km: f32,
    /// How far (km) from the grid origin the search is allowed to wander. The
    /// synthesis is already built around the storm, so the true center is
    /// near the origin; this keeps the search from locking onto an outer
    /// rainband's rotation.
    pub max_offset_km: f32,
    /// A level whose best azimuthal-mean tangential wind is below this (m/s)
    /// is reported center-less rather than fitting noise.
    pub min_vtan_ms: f32,
    /// Minimum number of valid grid points in the annulus for a candidate
    /// center to be considered — guards against a sparse, ill-constrained fit.
    pub min_points: usize,
    /// Level-to-level continuity: once an anchor level's center is fixed, each
    /// neighbouring level is searched only within this radius (km) of the
    /// already-accepted neighbour, so the derived center track follows the
    /// vortex's smooth tilt instead of jumping between rainband circulations.
    pub continuity_km: f32,
}

impl Default for CenterParams {
    fn default() -> Self {
        Self { rmin_km: 2.0, rmax_km: 50.0, max_offset_km: 50.0, min_vtan_ms: 3.0, min_points: 30, continuity_km: 12.0 }
    }
}

/// Tangential (cyclonic-positive) component of `(u, v)` at a point offset
/// `(dx, dy)` from the center, given radius `r = hypot(dx, dy)`. Derived by
/// dotting the wind with the counter-clockwise tangential unit vector
/// `(-dy, dx)/r`, so a purely rotational NH-cyclonic flow returns `> 0`.
#[inline]
fn tangential_component(u: f32, v: f32, dx: f32, dy: f32, r: f32) -> f32 {
    (v * dx - u * dy) / r
}

/// Valid `(x, y, u, v)` grid points within `max_r` km of the origin — the only
/// points any candidate center in `[-max_offset, max_offset]` with annulus out
/// to `rmax` can ever reach, so pre-filtering here turns each objective
/// evaluation from a full-grid scan (the xy grid spans ±250 km) into a short
/// loop over just the near-storm points.
fn collect_points(
    u: &[Vec<Option<f32>>],
    v: &[Vec<Option<f32>>],
    xs: &[f32],
    ys: &[f32],
    max_r: f32,
) -> Vec<(f32, f32, f32, f32)> {
    let mut pts = Vec::new();
    for (yi, &y) in ys.iter().enumerate() {
        if y.abs() > max_r {
            continue;
        }
        let (Some(urow), Some(vrow)) = (u.get(yi), v.get(yi)) else { continue };
        for (xi, &x) in xs.iter().enumerate() {
            if x.abs() > max_r {
                continue;
            }
            if let (Some(uu), Some(vv)) = (urow.get(xi).copied().flatten(), vrow.get(xi).copied().flatten()) {
                pts.push((x, y, uu, vv));
            }
        }
    }
    pts
}

/// Azimuthal-mean tangential wind over the annulus about `(cx, cy)`, plus the
/// count of contributing points (for the coverage floor).
fn mean_tangential(pts: &[(f32, f32, f32, f32)], cx: f32, cy: f32, rmin: f32, rmax: f32) -> (f32, usize) {
    let mut sum = 0.0f32;
    let mut n = 0usize;
    for &(x, y, u, v) in pts {
        let (dx, dy) = (x - cx, y - cy);
        let r = (dx * dx + dy * dy).sqrt();
        if r < rmin || r > rmax {
            continue;
        }
        sum += tangential_component(u, v, dx, dy, r);
        n += 1;
    }
    if n == 0 {
        (0.0, 0)
    } else {
        (sum / n as f32, n)
    }
}

/// Radius (km) of peak azimuthal-mean tangential wind about `(cx, cy)` — bins
/// the annulus points by radius (2 km bins) and returns the bin center with
/// the largest mean. `None` if no bin had data.
fn radius_of_max_wind(pts: &[(f32, f32, f32, f32)], cx: f32, cy: f32, rmin: f32, rmax: f32) -> Option<f32> {
    const BIN: f32 = 2.0;
    let nbins = (((rmax - rmin) / BIN).ceil() as usize).max(1);
    let mut sum = vec![0.0f32; nbins];
    let mut cnt = vec![0u32; nbins];
    for &(x, y, u, v) in pts {
        let (dx, dy) = (x - cx, y - cy);
        let r = (dx * dx + dy * dy).sqrt();
        if r < rmin || r > rmax {
            continue;
        }
        let b = (((r - rmin) / BIN) as usize).min(nbins - 1);
        sum[b] += tangential_component(u, v, dx, dy, r);
        cnt[b] += 1;
    }
    let mut best: Option<(f32, f32)> = None; // (radius, mean_vt)
    for b in 0..nbins {
        if cnt[b] == 0 {
            continue;
        }
        let mean = sum[b] / cnt[b] as f32;
        let radius = rmin + (b as f32 + 0.5) * BIN;
        if best.map(|(_, m)| mean > m).unwrap_or(true) {
            best = Some((radius, mean));
        }
    }
    best.map(|(r, _)| r)
}

/// Coarse-to-fine search for the center that maximizes [`mean_tangential`]
/// within `±window` km of `(cx0, cy0)`, hard-clamped to `±max_offset_km` of the
/// origin. Returns `(cx, cy, vtan)` or `None` if no candidate met the coverage
/// floor / `min_vtan_ms`. Deterministic (grid refinement, no randomness): scan
/// the window, then repeatedly zoom into the winning cell at ¼ the step until
/// ~100 m resolution. A full-domain fit passes `window = max_offset_km` and
/// `(0,0)`; a continuity-constrained fit passes a small `window` around the
/// neighbouring level's center.
fn search_window(pts: &[(f32, f32, f32, f32)], p: &CenterParams, cx0: f32, cy0: f32, window: f32) -> Option<(f32, f32, f32)> {
    let mo = p.max_offset_km;
    let (mut cx, mut cy) = (cx0, cy0);
    let mut w = window;
    let mut step = (window / 12.0).max(0.25);
    let mut best: Option<(f32, f32, f32)> = None;

    loop {
        let mut local: Option<(f32, f32, f32)> = None;
        let n = (2.0 * w / step).ceil() as i32;
        for iy in 0..=n {
            let cyc = (cy - w + iy as f32 * step).clamp(-mo, mo);
            for ix in 0..=n {
                let cxc = (cx - w + ix as f32 * step).clamp(-mo, mo);
                let (vt, cnt) = mean_tangential(pts, cxc, cyc, p.rmin_km, p.rmax_km);
                if cnt >= p.min_points && local.map(|(_, _, m)| vt > m).unwrap_or(true) {
                    local = Some((cxc, cyc, vt));
                }
            }
        }
        let (bx, by, bvt) = local?;
        best = Some((bx, by, bvt));
        cx = bx;
        cy = by;
        if step <= 0.1 {
            break;
        }
        w = step * 1.5;
        step /= 4.0;
    }

    best.filter(|(_, _, vt)| *vt >= p.min_vtan_ms)
}

/// A resolved center for one level, before it's turned into a [`LevelCenter`].
#[derive(Clone, Copy)]
struct Fit {
    cx: f32,
    cy: f32,
    vt: f32,
    rmw: Option<f32>,
}

/// Whether a fit is untrustworthy because the search ran into a limit rather
/// than settling on a real circulation: the center pinned against the
/// `±max_offset_km` search boundary (it wanted to drift further), or the radius
/// of maximum wind pinned at the outer annulus edge (the annulus never
/// contained the wind maximum — the "vortex" is really large-scale outer flow).
/// Such levels are reported center-less instead of publishing a boundary fit.
fn edge_pinned(f: &Fit, p: &CenterParams) -> bool {
    let at_offset = f.cx.abs() >= p.max_offset_km - 0.5 || f.cy.abs() >= p.max_offset_km - 0.5;
    let rmw_at_edge = f.rmw.map(|r| r >= p.rmax_km - 2.0).unwrap_or(true);
    at_offset || rmw_at_edge
}

/// Resolve a full-domain fit for one level's points, keeping it only if it
/// isn't edge-pinned.
fn independent_fit(pts: &[(f32, f32, f32, f32)], p: &CenterParams) -> Option<Fit> {
    let (cx, cy, vt) = search_window(pts, p, 0.0, 0.0, p.max_offset_km)?;
    let f = Fit { cx, cy, vt, rmw: radius_of_max_wind(pts, cx, cy, p.rmin_km, p.rmax_km) };
    (!edge_pinned(&f, p)).then_some(f)
}

/// Resolve a continuity-constrained fit near an already-accepted neighbour,
/// keeping it only if it isn't edge-pinned.
fn seeded_fit(pts: &[(f32, f32, f32, f32)], p: &CenterParams, sx: f32, sy: f32) -> Option<Fit> {
    let (cx, cy, vt) = search_window(pts, p, sx, sy, p.continuity_km)?;
    let f = Fit { cx, cy, vt, rmw: radius_of_max_wind(pts, cx, cy, p.rmin_km, p.rmax_km) };
    (!edge_pinned(&f, p)).then_some(f)
}

/// Per-level TDR-derived storm centers from the analysis wind volumes.
/// `u_volume`/`v_volume` are `[level][yi][xi]` (as [`xy_volume`] produces) for
/// the `U`/`V` fields; `xs`/`ys` the km grid; `levels` the CAPPI heights (km).
///
/// Two-stage so the track follows the vortex's physical tilt instead of hopping
/// between circulations:
/// 1. Fit every level independently over the whole search domain and discard
///    edge-pinned fits (see [`edge_pinned`]). The surviving level with the
///    strongest symmetric tangential wind is the **anchor** — the height where
///    the circulation is best defined.
/// 2. Propagate outward from the anchor (up, then down), re-fitting each level
///    within `continuity_km` of the neighbour just accepted. A level whose
///    constrained fit is weak or edge-pinned is reported center-less, but the
///    last good center still seeds the next level so one data-poor layer
///    doesn't break the chain.
///
/// If no level yields a trustworthy anchor (a weak or disorganized storm), all
/// levels come back center-less — deliberately, rather than publishing noise.
pub fn tangential_wind_centers(
    u_volume: &[Vec<Vec<Option<f32>>>],
    v_volume: &[Vec<Vec<Option<f32>>>],
    xs: &[f32],
    ys: &[f32],
    levels: &[f32],
    params: &CenterParams,
) -> Vec<LevelCenter> {
    let max_r = params.max_offset_km + params.rmax_km;
    let nlev = levels.len();
    let pts: Vec<Vec<(f32, f32, f32, f32)>> = (0..nlev)
        .map(|li| match (u_volume.get(li), v_volume.get(li)) {
            (Some(u), Some(v)) => collect_points(u, v, xs, ys, max_r),
            _ => Vec::new(),
        })
        .collect();

    let independent: Vec<Option<Fit>> = pts.iter().map(|p| independent_fit(p, params)).collect();

    // Anchor = strongest trustworthy level.
    let anchor = independent
        .iter()
        .enumerate()
        .filter_map(|(i, f)| f.map(|f| (i, f.vt)))
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
        .map(|(i, _)| i);

    let mut accepted: Vec<Option<Fit>> = vec![None; nlev];
    if let Some(a) = anchor {
        accepted[a] = independent[a];
        let mut last = independent[a].unwrap();
        for li in (a + 1)..nlev {
            if let Some(f) = seeded_fit(&pts[li], params, last.cx, last.cy) {
                accepted[li] = Some(f);
                last = f;
            }
        }
        let mut last = independent[a].unwrap();
        for li in (0..a).rev() {
            if let Some(f) = seeded_fit(&pts[li], params, last.cx, last.cy) {
                accepted[li] = Some(f);
                last = f;
            }
        }
    }

    levels
        .iter()
        .enumerate()
        .map(|(li, &level_km)| match accepted[li] {
            Some(f) => LevelCenter {
                level_km,
                x_km: Some(f.cx),
                y_km: Some(f.cy),
                vtan_ms: Some(f.vt),
                rmw_km: f.rmw,
            },
            None => LevelCenter { level_km, x_km: None, y_km: None, vtan_ms: None, rmw_km: None },
        })
        .collect()
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
        let mosaic = geo_mosaic(&planes, CombineMode::Max);
        // Combined x extent: plane a covers [0,1], plane b (shifted) covers [1,2] -> [0,1,2].
        assert_eq!(mosaic.x, vec![0.0, 1.0, 2.0]);
        assert_eq!(mosaic.y, vec![0.0]);
        // col0 (x=0): only a's x=0 -> 3.0. col1 (x=1): a's x=1 (missing) + b's x=0 (5.0) -> 5.0.
        // col2 (x=2): only b's x=1 -> 9.0.
        assert_eq!(mosaic.data[0], vec![Some(3.0), Some(5.0), Some(9.0)]);
    }

    #[test]
    fn geo_mosaic_mean_averages_overlapping_cells_instead_of_maxing() {
        // Same layout as the Max test above, but col1 (x=1) is where a's
        // missing value and b's 5.0 would collide with a *second* real
        // reading — use two planes that both have real data at the same
        // output cell so Mean has something to average.
        let x = [0.0f32, 1.0];
        let y = [0.0f32];
        let a = vec![vec![Some(3.0), Some(7.0)]];
        let b = vec![vec![Some(5.0), Some(9.0)]];
        let planes = vec![
            GeoPlane { x: &x, y: &y, data: &a, offset_x_km: 0.0, offset_y_km: 0.0 },
            GeoPlane { x: &x, y: &y, data: &b, offset_x_km: 1.0, offset_y_km: 0.0 },
        ];
        let mosaic = geo_mosaic(&planes, CombineMode::Mean);
        // col1 (x=1) is the overlap: a's x=1 (7.0) and b's x=0 (5.0) -> mean 6.0.
        assert_eq!(mosaic.data[0], vec![Some(3.0), Some(6.0), Some(9.0)]);
    }

    #[test]
    fn geo_mosaic_empty_input_returns_empty() {
        let mosaic = geo_mosaic(&[], CombineMode::Max);
        assert!(mosaic.x.is_empty() && mosaic.y.is_empty() && mosaic.data.is_empty());
    }

    #[test]
    fn combine_mode_for_field_is_max_for_reflectivity_mean_otherwise() {
        assert_eq!(combine_mode_for_field("reflectivity"), CombineMode::Max);
        assert_eq!(combine_mode_for_field("wind_speed"), CombineMode::Mean);
        assert_eq!(combine_mode_for_field("radial_wind"), CombineMode::Mean);
        assert_eq!(combine_mode_for_field("vort"), CombineMode::Mean);
    }

    #[test]
    fn bilinear_sample_interpolates_at_midpoint_and_matches_grid_at_nodes() {
        let xs = [0.0f32, 2.0];
        let ys = [0.0f32, 2.0];
        // data[yi][xi]: (0,0)=0, (0,1)=10, (1,0)=20, (1,1)=30.
        let data = vec![vec![Some(0.0), Some(10.0)], vec![Some(20.0), Some(30.0)]];
        assert_eq!(bilinear_sample(&data, &xs, &ys, 0.0, 0.0), Some(0.0));
        assert_eq!(bilinear_sample(&data, &xs, &ys, 2.0, 2.0), Some(30.0));
        // Center of the cell is the average of all 4 corners.
        assert_eq!(bilinear_sample(&data, &xs, &ys, 1.0, 1.0), Some(15.0));
        // Outside the grid entirely.
        assert_eq!(bilinear_sample(&data, &xs, &ys, 5.0, 5.0), None);
    }

    #[test]
    fn bilinear_sample_renormalizes_around_a_missing_corner() {
        let xs = [0.0f32, 2.0];
        let ys = [0.0f32, 2.0];
        // Corner (1,1) missing; the rest are 10.0, so any point should
        // still resolve to 10.0 once weights are renormalized.
        let data = vec![vec![Some(10.0), Some(10.0)], vec![Some(10.0), None]];
        assert_eq!(bilinear_sample(&data, &xs, &ys, 1.5, 1.5), Some(10.0));
        // All 4 corners missing -> None.
        let all_missing = vec![vec![None, None], vec![None, None]];
        assert_eq!(bilinear_sample(&all_missing, &xs, &ys, 1.0, 1.0), None);
    }

    #[test]
    fn plane_slice_cuts_a_diagonal_cross_section_through_a_volume() {
        let xs = [0.0f32, 2.0];
        let ys = [0.0f32, 2.0];
        let levels = [0.0f32, 1.0];
        // Level 0: all zeros. Level 1: a simple ramp 0/10/20/30.
        let level0 = vec![vec![Some(0.0), Some(0.0)], vec![Some(0.0), Some(0.0)]];
        let level1 = vec![vec![Some(0.0), Some(10.0)], vec![Some(20.0), Some(30.0)]];
        let volume = vec![level0, level1];

        let cut = plane_slice(&volume, &xs, &ys, &levels, 0.0, 0.0, 2.0, 2.0, 3);
        assert_eq!(cut.levels, vec![0.0, 1.0]);
        // 3 samples along a 2*sqrt(2)-km diagonal: 0, half, full length.
        assert!((cut.along_km[0]).abs() < 1e-4);
        assert!((cut.along_km[2] - (8.0f32).sqrt()).abs() < 1e-3);
        // Level 0 is flat zero everywhere along the cut.
        assert_eq!(cut.data[0], vec![Some(0.0), Some(0.0), Some(0.0)]);
        // Level 1 walks the diagonal from corner (0.0) through the center
        // (mean of all 4 corners = 15.0) to the far corner (30.0).
        assert_eq!(cut.data[1], vec![Some(0.0), Some(15.0), Some(30.0)]);
    }

    #[test]
    fn nearest_index_picks_closest() {
        let levels = [0.0, 0.5, 1.0, 1.5, 2.0];
        assert_eq!(nearest_index(&levels, 1.6), 3);
        assert_eq!(nearest_index(&levels, -5.0), 0);
        assert_eq!(nearest_index(&levels, 50.0), 4);
    }

    #[test]
    fn latlon_from_offset_km_round_trips_with_latlon_offset_km() {
        let (lat0, lon0) = (29.5f32, -88.4f32);
        // A known offset back to lat/lon, then forward again, lands where it started.
        let (lat, lon) = latlon_from_offset_km(37.0, -22.0, lat0, lon0);
        let (dx, dy) = latlon_offset_km(lat, lon, lat0, lon0);
        assert!((dx - 37.0).abs() < 1e-2, "dx={dx}");
        assert!((dy + 22.0).abs() < 1e-2, "dy={dy}");
        // Zero offset is the reference point exactly.
        let (lat, lon) = latlon_from_offset_km(0.0, 0.0, lat0, lon0);
        assert!((lat - lat0).abs() < 1e-4 && (lon - lon0).abs() < 1e-4);
    }

    /// One level's `(u, v)` planes for an idealized cyclonic (CCW) Rankine
    /// vortex centered at `(cx, cy)` km on the given coordinate grid.
    fn rankine_planes(coords: &[f32], cx: f32, cy: f32, rmw: f32, vmax: f32) -> (Vec<Vec<Option<f32>>>, Vec<Vec<Option<f32>>>) {
        let n = coords.len();
        let mut u = vec![vec![None; n]; n];
        let mut v = vec![vec![None; n]; n];
        for (yi, &y) in coords.iter().enumerate() {
            for (xi, &x) in coords.iter().enumerate() {
                let (dx, dy) = (x - cx, y - cy);
                let r = (dx * dx + dy * dy).sqrt();
                if r < 1e-3 {
                    u[yi][xi] = Some(0.0);
                    v[yi][xi] = Some(0.0);
                    continue;
                }
                let vt = if r <= rmw { vmax * (r / rmw) } else { vmax * (rmw / r) };
                // velocity = vt * CCW-tangential unit (-dy, dx)/r
                u[yi][xi] = Some(-vt * dy / r);
                v[yi][xi] = Some(vt * dx / r);
            }
        }
        (u, v)
    }

    fn test_params() -> CenterParams {
        CenterParams { rmin_km: 2.0, rmax_km: 30.0, max_offset_km: 25.0, min_vtan_ms: 3.0, min_points: 10, continuity_km: 10.0 }
    }

    #[test]
    fn tangential_wind_centers_recovers_an_offset_vortex_center() {
        let coords: Vec<f32> = (0..=40).map(|i| -40.0 + i as f32 * 2.0).collect();
        let (u, v) = rankine_planes(&coords, 6.0, -4.0, 15.0, 40.0);
        let centers = tangential_wind_centers(&[u], &[v], &coords, &coords, &[2.0], &test_params());
        assert_eq!(centers.len(), 1);
        let c = &centers[0];
        let (x, y) = (c.x_km.unwrap(), c.y_km.unwrap());
        assert!((x - 6.0).abs() < 1.0, "recovered x={x}, want ~6");
        assert!((y + 4.0).abs() < 1.0, "recovered y={y}, want ~-4");
        // A symmetric vortex's azimuthal-mean tangential wind at its true
        // center is strongly positive (cyclonic).
        assert!(c.vtan_ms.unwrap() > 10.0);
        // RMW should land near the imposed 15 km (within one 2-km bin or two).
        assert!((c.rmw_km.unwrap() - 15.0).abs() <= 4.0, "rmw={:?}", c.rmw_km);
    }

    #[test]
    fn tangential_wind_centers_follows_a_tilted_vortex_via_continuity() {
        // Three levels whose centers march east with height: (0,0)->(4,0)->(8,0).
        let coords: Vec<f32> = (0..=40).map(|i| -40.0 + i as f32 * 2.0).collect();
        let want = [(0.0f32, 0.0f32), (4.0, 0.0), (8.0, 0.0)];
        let (mut uvol, mut vvol) = (Vec::new(), Vec::new());
        for &(cx, cy) in &want {
            let (u, v) = rankine_planes(&coords, cx, cy, 15.0, 40.0);
            uvol.push(u);
            vvol.push(v);
        }
        let centers = tangential_wind_centers(&uvol, &vvol, &coords, &coords, &[1.0, 2.0, 3.0], &test_params());
        for (c, &(wx, wy)) in centers.iter().zip(&want) {
            let (x, y) = (c.x_km.unwrap(), c.y_km.unwrap());
            assert!((x - wx).abs() < 1.5 && (y - wy).abs() < 1.5, "level {} got ({x},{y}) want ({wx},{wy})", c.level_km);
        }
        // Recovered track is monotonic in x, i.e. it followed the tilt.
        assert!(centers[0].x_km.unwrap() < centers[1].x_km.unwrap());
        assert!(centers[1].x_km.unwrap() < centers[2].x_km.unwrap());
    }

    #[test]
    fn tangential_wind_centers_reports_none_for_a_calm_level() {
        // Zero wind everywhere -> no circulation -> center-less.
        let coords: Vec<f32> = (0..=40).map(|i| -40.0 + i as f32 * 2.0).collect();
        let plane = vec![vec![Some(0.0); coords.len()]; coords.len()];
        let u_volume = vec![plane.clone()];
        let v_volume = vec![plane];
        let centers =
            tangential_wind_centers(&u_volume, &v_volume, &coords, &coords, &[2.0], &CenterParams::default());
        assert!(centers[0].x_km.is_none() && centers[0].vtan_ms.is_none());
    }
}
