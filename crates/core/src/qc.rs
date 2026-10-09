//! "Custom QC" — an experimental, second QC pass for TDR Level 1b data, run
//! entirely on the already-synthesized grid (`FieldSlice`/`FieldVolume` in
//! `services/tdr_nc.rs`), not on raw Doppler radials.
//!
//! Every documented NOAA/HRD QC step (dealiasing, sea-clutter/sidelobe
//! removal, spectral-width gating, dual-PRF range-ambiguity correction — see
//! Gamache 2005's JHT report and AOML's 2023 dual-PRF science doc) runs on
//! raw radials or individual sweeps, upstream of or during HRD's 3D-
//! variational synthesis. This API never downloads raw radials — only the
//! finished gridded product (`services/tdr_ingest.rs`'s doc comment) — so
//! this module is a genuinely complementary second pass: it catches
//! synthesis-stage artifacts (grid-cell spikes, vertical discontinuities,
//! azimuthal stitching seams, low-coverage edges, storm-relative-transform
//! errors) that no raw-radial QC could ever see, since they only exist after
//! the variational solve. It cannot correct anything upstream.
//!
//! Checks A-E are robust (median/MAD-based) statistical outlier tests
//! against the cell's own local context — never a fixed physical threshold —
//! so they self-calibrate per mission/field instead of assuming one hardcoded
//! number is right for a category-1 depression and a category-5 eyewall
//! alike. Check F ([`qc_wind_clutter`]) is the one physical-threshold check:
//! phantom weak echoes under strong winds sit at a known, narrow dBZ band
//! that a local-outlier test can't see (they come in coherent patches, so
//! they *are* their own local context). A flagged cell is set to `None`, never interpolated or fabricated,
//! matching the missing-value convention already used throughout
//! `services/tdr_nc.rs`.
//!
//! Pure array math, no I/O — same WASM-safe constraint as `sweep.rs`. The
//! server (`services/tdr_nc.rs`) does the netCDF decode and any extra file
//! fetch (the D check's `xy_rel` counterpart) and hands already-decoded
//! grids in here.

use serde::Serialize;

/// Scales MAD (median absolute deviation) into a consistent estimator of
/// standard deviation for normally-distributed data — the standard
/// robust-statistics convention (not TDR-specific), so `mad_k` reads like a
/// familiar sigma multiple.
const MAD_SCALE: f32 = 1.4826;

/// Tunables shared by every check. Defaults are general robust-statistics
/// conventions, not values pulled from the TDR literature — see the module
/// doc comment on why every check here is necessarily *new* territory (no
/// paper describes QC on the finished grid to calibrate against).
#[derive(Clone, Copy, Debug, Serialize)]
pub struct QcParams {
    /// Outlier threshold, in MAD units, shared by despiking, vertical
    /// continuity, the azimuthal-ring check, and cross-consistency. Default
    /// 3.5 — Iglewicz & Hoaglin's standard modified-z-score cutoff.
    pub mad_k: f32,
    /// Spatial despiking neighborhood half-width in cells (default 2 -> a
    /// 5x5 window around each cell).
    pub window: usize,
    /// Valid neighbors required before a cell is despike-tested (spatial
    /// window), azimuthal-ring-tested (per annulus bin), or
    /// cross-consistency-tested (per plane) — below this, there simply
    /// isn't enough context for a trustworthy robust-stats estimate, so the
    /// cell is left untouched by that check rather than guessed at.
    pub min_neighbors: usize,
    /// Valid-neighbor floor (spatial window) below which a cell is trimmed
    /// as low-confidence regardless of its value — targets the exact
    /// weakness Gamache's own report calls out: automatic QC loses more
    /// inner-eyewall-edge coverage than manual QC, so remaining low-coverage
    /// cells are flagged rather than trusted. Must be <= `min_neighbors`, or
    /// every cell either gets edge-trimmed or despike-tested with nothing
    /// in between.
    pub min_coverage: usize,
    /// Azimuthal-ring annulus width in km (default 2.0 — matches the
    /// existing radius-of-max-wind binning convention in `sweep.rs`).
    pub ring_width_km: f32,
    /// F (wind clutter): run the check at all. Default true.
    pub clutter_enabled: bool,
    /// F (wind clutter): wind speed (m/s) at or above which a weak echo in
    /// the same cell is treated as a phantom return. Default 25 m/s (~50 kt).
    pub clutter_min_wind_ms: f32,
    /// F: when > 0, the wind threshold drops to this fraction of the plane's
    /// peak wind speed if that's lower than `clutter_min_wind_ms` — so a
    /// weaker storm's "strong" winds still count. Default 0 (off).
    pub clutter_wind_frac_of_peak: f32,
    /// F: when > 0, a weak echo with no wind value of its own borrows the
    /// strongest wind within this many km. The synthesis often keeps phantom
    /// reflectivity while producing no wind there at all, so without this
    /// those cells are never tested. Default 0 (off).
    pub clutter_wind_search_km: f32,
    /// F: when set, the check only runs on CAPPI levels at or below this
    /// height (km), sparing weak upper-level echo. Default unset (all
    /// levels). Ignored for an altitude composite, which has no single level.
    pub clutter_max_height_km: Option<f32>,
    /// F: the dBZ cutoff for a weak storm — used when the plane's genuine
    /// echoes have a median at or below `clutter_ref_low_dbz`. Default 2.0,
    /// the top of the 0-2 dBZ band the phantoms usually sit in.
    pub clutter_cutoff_low_dbz: f32,
    /// F: the dBZ cutoff for a strong storm — used when the plane's genuine
    /// echoes have a median at or above `clutter_ref_high_dbz`. Default 5.0,
    /// the observed upper bound of the phantoms. Also the floor a cell must
    /// exceed to count as a "genuine echo" in that median.
    pub clutter_cutoff_high_dbz: f32,
    /// F: median genuine-echo reflectivity (dBZ) at or below which the low
    /// cutoff applies. Default 15.
    pub clutter_ref_low_dbz: f32,
    /// F: median genuine-echo reflectivity (dBZ) at or above which the high
    /// cutoff applies; linear in between. Default 30.
    pub clutter_ref_high_dbz: f32,
}

impl Default for QcParams {
    fn default() -> Self {
        Self {
            mad_k: 3.5,
            window: 2,
            min_neighbors: 8,
            min_coverage: 4,
            ring_width_km: 2.0,
            clutter_enabled: true,
            clutter_min_wind_ms: 25.0,
            clutter_wind_frac_of_peak: 0.0,
            clutter_wind_search_km: 0.0,
            clutter_max_height_km: None,
            clutter_cutoff_low_dbz: 2.0,
            clutter_cutoff_high_dbz: 5.0,
            clutter_ref_low_dbz: 15.0,
            clutter_ref_high_dbz: 30.0,
        }
    }
}

/// Per-check flagged-cell counts from one QC pass, for the API response's
/// `qc_summary` — lets a caller see how aggressively a given grid was
/// edited without having to diff it against the un-QC'd version themselves.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct QcReport {
    /// Cells that were non-missing before this pass started.
    pub cells_examined: usize,
    pub cells_flagged_despike: usize,
    pub cells_flagged_vertical: usize,
    pub cells_flagged_azimuthal: usize,
    pub cells_flagged_cross_consistency: usize,
    pub cells_flagged_edge: usize,
    pub cells_flagged_wind_clutter: usize,
}

impl QcReport {
    /// Accumulates another pass's counts into this one — e.g. summing
    /// per-analysis-time QC results across a composite's constituent files.
    pub fn merge(&mut self, other: QcReport) {
        self.cells_examined += other.cells_examined;
        self.cells_flagged_despike += other.cells_flagged_despike;
        self.cells_flagged_vertical += other.cells_flagged_vertical;
        self.cells_flagged_azimuthal += other.cells_flagged_azimuthal;
        self.cells_flagged_cross_consistency += other.cells_flagged_cross_consistency;
        self.cells_flagged_edge += other.cells_flagged_edge;
        self.cells_flagged_wind_clutter += other.cells_flagged_wind_clutter;
    }
}

pub fn count_valid(data: &[Vec<Option<f32>>]) -> usize {
    data.iter().flatten().filter(|v| v.is_some()).count()
}

/// Sorts `values` in place and returns the median. `NaN` for an empty slice
/// (never called that way here — every call site checks length first).
fn median_sorted(values: &mut [f32]) -> f32 {
    if values.is_empty() {
        return f32::NAN;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = values.len();
    if n % 2 == 1 {
        values[n / 2]
    } else {
        (values[n / 2 - 1] + values[n / 2]) / 2.0
    }
}

/// Median absolute deviation from an already-known median. `values`' order
/// doesn't matter (and may already be scrambled by a prior `median_sorted`
/// call over the same slice — that's fine, MAD only needs the multiset).
fn mad_of(values: &[f32], med: f32) -> f32 {
    let mut deviations: Vec<f32> = values.iter().map(|v| (v - med).abs()).collect();
    median_sorted(&mut deviations)
}

/// Robust outlier test: is `value` more than `k` MAD-scaled deviations from
/// `med`? A MAD-scaled test is meaningless when `spread` is (near) zero — a
/// degenerate, perfectly-uniform baseline — so that case falls back to a
/// tiny absolute/relative tolerance instead: with zero natural variance,
/// *any* real deviation is unambiguous (no threshold to calibrate), but a
/// bit-for-bit-equal comparison would flag harmless floating-point noise, so
/// a small floor absorbs that without needing MAD to calibrate it.
fn is_outlier(value: f32, med: f32, spread: f32, k: f32) -> bool {
    if spread > f32::EPSILON {
        (value - med).abs() > k * MAD_SCALE * spread
    } else {
        (value - med).abs() > (med.abs() * 1e-4).max(1e-3)
    }
}

// ── A + E: spatial despiking + low-coverage edge trim ───────────────────────
// One combined pass (same neighbor-collection loop) since both need the same
// per-cell neighbor list. Two-pass internally (flag, then mask) so a cell
// masked early in the scan can't change its not-yet-visited neighbors'
// verdicts mid-pass.
fn spatial_pass(data: &mut [Vec<Option<f32>>], params: &QcParams) -> (usize, usize) {
    let ny = data.len();
    if ny == 0 {
        return (0, 0);
    }
    let nx = data[0].len();
    let w = params.window as i32;

    let mut despike_flags = vec![vec![false; nx]; ny];
    let mut edge_flags = vec![vec![false; nx]; ny];
    for yi in 0..ny {
        for xi in 0..nx {
            let Some(center) = data[yi][xi] else { continue };
            let mut neighbors = Vec::new();
            for dy in -w..=w {
                for dx in -w..=w {
                    if dx == 0 && dy == 0 {
                        continue;
                    }
                    let (nyi, nxi) = (yi as i32 + dy, xi as i32 + dx);
                    if nyi < 0 || nxi < 0 || nyi as usize >= ny || nxi as usize >= nx {
                        continue;
                    }
                    if let Some(v) = data[nyi as usize][nxi as usize] {
                        neighbors.push(v);
                    }
                }
            }
            if neighbors.len() < params.min_coverage {
                edge_flags[yi][xi] = true;
                continue;
            }
            if neighbors.len() < params.min_neighbors {
                continue; // enough to not be "low-coverage", not enough for a trustworthy despike test
            }
            let med = median_sorted(&mut neighbors);
            let spread = mad_of(&neighbors, med);
            if is_outlier(center, med, spread, params.mad_k) {
                despike_flags[yi][xi] = true;
            }
        }
    }

    let (mut despiked, mut edged) = (0, 0);
    for yi in 0..ny {
        for xi in 0..nx {
            if edge_flags[yi][xi] {
                data[yi][xi] = None;
                edged += 1;
            } else if despike_flags[yi][xi] {
                data[yi][xi] = None;
                despiked += 1;
            }
        }
    }
    (despiked, edged)
}

// ── C: azimuthal-ring consistency (xy-family planes only) ───────────────────
// Bins around grid-origin (0,0) rather than a re-derived storm center: the
// xy/xy_rel grids are already built centered on the storm's position at that
// analysis time (see the dashboard's own product-help copy), so (0,0) is
// already storm-centered by construction. Reusing `sweep::tangential_wind_
// centers` for a height-varying, tilt-aware center would be more precise but
// needs a second U/V fetch+decode to QC an unrelated field like reflectivity
// — a plausible future precision upgrade, not required here.
fn azimuthal_pass(data: &mut [Vec<Option<f32>>], x: &[f32], y: &[f32], params: &QcParams) -> usize {
    let ny = data.len();
    if ny == 0 || x.is_empty() || y.is_empty() {
        return 0;
    }
    let nx = data[0].len();

    let bin_of = |r: f32| (r / params.ring_width_km).floor().max(0.0) as usize;
    let mut bin_values: Vec<Vec<f32>> = Vec::new();
    let mut bin_index = vec![vec![None::<usize>; nx]; ny];
    for (yi, &yv) in y.iter().enumerate().take(ny) {
        for (xi, &xv) in x.iter().enumerate().take(nx) {
            if let Some(v) = data[yi][xi] {
                let b = bin_of((xv * xv + yv * yv).sqrt());
                if bin_values.len() <= b {
                    bin_values.resize(b + 1, Vec::new());
                }
                bin_values[b].push(v);
                bin_index[yi][xi] = Some(b);
            }
        }
    }

    let bin_stats: Vec<Option<(f32, f32)>> = bin_values
        .into_iter()
        .map(|mut vals| {
            if vals.len() < params.min_neighbors {
                return None;
            }
            let med = median_sorted(&mut vals);
            Some((med, mad_of(&vals, med)))
        })
        .collect();

    let mut flagged = 0;
    for yi in 0..ny {
        for xi in 0..nx {
            let Some(b) = bin_index[yi][xi] else { continue };
            let Some((med, spread)) = bin_stats[b] else { continue };
            // Still `Some` here: this cell hasn't been touched since the
            // collection pass above (each cell is visited once in this loop).
            let v = data[yi][xi].expect("bin_index only set for originally-valid cells");
            if is_outlier(v, med, spread, params.mad_k) {
                data[yi][xi] = None;
                flagged += 1;
            }
        }
    }
    flagged
}

/// Despike + azimuthal-ring + edge trim on one CAPPI level (`data[yi][xi]`,
/// `x`/`y` the grid's km-from-origin coordinates — same orientation as
/// `sweep::cappi_slice`). This is the entry point for a single-level `xy`
/// sweep (`GET /v1/tdr/sweep`); [`qc_volume`] calls it per level for a full
/// volume.
pub fn qc_plane_xy(data: &mut [Vec<Option<f32>>], x: &[f32], y: &[f32], params: &QcParams) -> QcReport {
    let mut report = QcReport { cells_examined: count_valid(data), ..Default::default() };
    let (despiked, edged) = spatial_pass(data, params);
    report.cells_flagged_despike = despiked;
    report.cells_flagged_edge = edged;
    report.cells_flagged_azimuthal = azimuthal_pass(data, x, y, params);
    report
}

/// Despike + edge trim only — no azimuthal ring, since a single `vert_*`
/// radial cross-section has no azimuthal dimension to bin by (its `x` axis
/// is already along-track radius, not km-from-center in two dimensions).
pub fn qc_plane_vert(data: &mut [Vec<Option<f32>>], params: &QcParams) -> QcReport {
    let mut report = QcReport { cells_examined: count_valid(data), ..Default::default() };
    let (despiked, edged) = spatial_pass(data, params);
    report.cells_flagged_despike = despiked;
    report.cells_flagged_edge = edged;
    report
}

// ── B: vertical continuity ───────────────────────────────────────────────
// Same MAD-outlier idea as spatial despiking, applied along the level axis
// instead of the x/y plane: a cell is compared against a small window of
// *vertically* neighboring levels at the same (x,y), not against a delta
// sequence — a single bad level then shows up directly as a local outlier
// against its vertical neighbors, with no delta-sign-interpretation puzzle
// to solve for where the badness actually is.
fn vertical_pass(data: &mut [Vec<Vec<Option<f32>>>], params: &QcParams) -> usize {
    let nlev = data.len();
    if nlev == 0 {
        return 0;
    }
    let ny = data[0].len();
    let nx = if ny > 0 { data[0][0].len() } else { 0 };
    let vwindow = params.window.max(1);

    let mut flags = vec![vec![vec![false; nx]; ny]; nlev];
    for yi in 0..ny {
        for xi in 0..nx {
            for li in 0..nlev {
                let Some(center) = data[li][yi][xi] else { continue };
                let mut neighbors = Vec::new();
                for dl in 1..=vwindow {
                    if li >= dl {
                        if let Some(v) = data[li - dl][yi][xi] {
                            neighbors.push(v);
                        }
                    }
                    if li + dl < nlev {
                        if let Some(v) = data[li + dl][yi][xi] {
                            neighbors.push(v);
                        }
                    }
                }
                // Deliberately looser than the spatial/azimuthal floor: a
                // volume may only have a handful of CAPPI levels at all, so
                // requiring `min_neighbors` here would silently disable the
                // check on most real missions.
                if neighbors.len() < 2 {
                    continue;
                }
                let med = median_sorted(&mut neighbors);
                let spread = mad_of(&neighbors, med);
                if is_outlier(center, med, spread, params.mad_k) {
                    flags[li][yi][xi] = true;
                }
            }
        }
    }

    let mut flagged = 0;
    for (li, level) in flags.into_iter().enumerate() {
        for (yi, row) in level.into_iter().enumerate() {
            for (xi, flag) in row.into_iter().enumerate() {
                if flag {
                    data[li][yi][xi] = None;
                    flagged += 1;
                }
            }
        }
    }
    flagged
}

/// Despike + azimuthal-ring + edge trim per level (`qc_plane_xy`), then a
/// vertical-continuity pass across the (now partially cleaned) level axis.
/// `data[level_idx][yi][xi]`, matching `sweep::xy_volume`'s orientation.
pub fn qc_volume(data: &mut [Vec<Vec<Option<f32>>>], x: &[f32], y: &[f32], params: &QcParams) -> QcReport {
    let mut report = QcReport::default();
    for level in data.iter_mut() {
        report.merge(qc_plane_xy(level, x, y, params));
    }
    report.cells_flagged_vertical = vertical_pass(data, params);
    report
}

// ── D: xy/xy_rel cross-consistency (opportunistic) ───────────────────────

/// Flags cells where `primary` departs from `counterpart` (the paired
/// xy/xy_rel file's decoded grid for the *same* field, at the *same*
/// analysis time/level) by more than the typical departure across the whole
/// plane. Using the plane's own *median* diff as the baseline — rather than
/// assuming the two grids should match exactly — handles both kinds of field
/// the two grids can differ by with one piece of code: `reflectivity`/`w`/
/// `vort` shouldn't differ at all (median diff ≈ 0, any real spread is
/// synthesis noise), while `u`/`v` legitimately differ by the storm's
/// translational motion (a genuine non-zero constant) — either way, a cell
/// whose diff departs from the plane's own typical diff is the anomaly, not
/// the constant itself. Skipped entirely for `radial_wind`/`tangential_wind`,
/// which are different geometric projections between the two grids by
/// definition, not related by a simple offset — the caller shouldn't call
/// this for those fields, but returning 0 rather than panicking keeps a
/// caller mistake harmless.
pub fn qc_cross_consistency(
    primary: &mut [Vec<Option<f32>>],
    counterpart: &[Vec<Option<f32>>],
    field: &str,
    params: &QcParams,
) -> usize {
    if matches!(field, "radial_wind" | "tangential_wind") {
        return 0;
    }
    let ny = primary.len();
    if ny == 0 || counterpart.len() != ny {
        return 0;
    }
    let nx = primary[0].len();

    let mut diffs = Vec::new();
    let mut diff_grid = vec![vec![None::<f32>; nx]; ny];
    for yi in 0..ny {
        if counterpart[yi].len() != nx {
            return 0; // mismatched grid shape — nothing safe to compare cell-for-cell
        }
        for xi in 0..nx {
            if let (Some(p), Some(c)) = (primary[yi][xi], counterpart[yi][xi]) {
                let d = p - c;
                diffs.push(d);
                diff_grid[yi][xi] = Some(d);
            }
        }
    }
    if diffs.len() < params.min_neighbors {
        return 0;
    }
    let med = median_sorted(&mut diffs);
    let spread = mad_of(&diffs, med);

    let mut flagged = 0;
    for yi in 0..ny {
        for xi in 0..nx {
            if let Some(d) = diff_grid[yi][xi] {
                if is_outlier(d, med, spread, params.mad_k) {
                    primary[yi][xi] = None;
                    flagged += 1;
                }
            }
        }
    }
    flagged
}

// ── F: wind-driven weak-echo clutter (reflectivity only) ─────────────────
// Strong winds produce phantom returns, typically 0-2 dBZ and almost never
// above 5 dBZ. They arrive as spatially coherent patches, so the MAD tests
// above treat them as normal local context. This check instead pairs each
// reflectivity cell with the wind speed at the same cell: weak echo + strong
// wind = phantom. How weak counts as "weak" scales with the storm — a weak
// system's real precipitation can legitimately sit near 5 dBZ, while in a
// strong system's high-wind region it's almost certainly clutter.

/// Whether check F applies to `field` — only reflectivity has phantom weak
/// echoes to remove. Callers use it to skip reading wind speed otherwise.
pub fn wind_clutter_applies(field: &str) -> bool {
    field == "reflectivity"
}

/// The dBZ cutoff for one reflectivity plane: the median of its genuine
/// echoes (cells above `clutter_cutoff_high_dbz`, so the phantoms being
/// hunted can't drag it down) mapped linearly from `clutter_ref_low_dbz` ->
/// `clutter_cutoff_low_dbz` to `clutter_ref_high_dbz` ->
/// `clutter_cutoff_high_dbz`, clamped at both ends. Falls back to the low
/// cutoff when there are fewer than `min_neighbors` genuine echoes to take a
/// median of.
pub fn wind_clutter_cutoff_dbz(reflectivity: &[Vec<Option<f32>>], params: &QcParams) -> f32 {
    let mut echoes: Vec<f32> =
        reflectivity.iter().flatten().filter_map(|v| *v).filter(|&v| v > params.clutter_cutoff_high_dbz).collect();
    if echoes.len() < params.min_neighbors {
        return params.clutter_cutoff_low_dbz;
    }
    let med = median_sorted(&mut echoes);
    let span = params.clutter_ref_high_dbz - params.clutter_ref_low_dbz;
    let t = if span > f32::EPSILON { ((med - params.clutter_ref_low_dbz) / span).clamp(0.0, 1.0) } else { 1.0 };
    params.clutter_cutoff_low_dbz + t * (params.clutter_cutoff_high_dbz - params.clutter_cutoff_low_dbz)
}

/// The wind speed (m/s) at or above which check F counts a cell's wind as
/// strong for this plane: `clutter_min_wind_ms`, lowered to
/// `clutter_wind_frac_of_peak` x the plane's peak wind when that's enabled
/// and smaller.
pub fn wind_clutter_threshold_ms(wind_speed: &[Vec<Option<f32>>], params: &QcParams) -> f32 {
    if params.clutter_wind_frac_of_peak <= 0.0 {
        return params.clutter_min_wind_ms;
    }
    let peak = wind_speed.iter().flatten().filter_map(|v| *v).fold(f32::NEG_INFINITY, f32::max);
    if peak.is_finite() {
        params.clutter_min_wind_ms.min(params.clutter_wind_frac_of_peak * peak)
    } else {
        params.clutter_min_wind_ms
    }
}

/// Strongest valid wind within `radius_km` of cell `(yi, xi)`, using the
/// grid's own km coordinates. `None` if there's none in range.
fn max_wind_within(wind_speed: &[Vec<Option<f32>>], x: &[f32], y: &[f32], yi: usize, xi: usize, radius_km: f32) -> Option<f32> {
    let step = |c: &[f32]| if c.len() > 1 { (c[1] - c[0]).abs().max(f32::EPSILON) } else { f32::INFINITY };
    let kx = (radius_km / step(x)).ceil().min(x.len() as f32) as usize;
    let ky = (radius_km / step(y)).ceil().min(y.len() as f32) as usize;
    let mut best: Option<f32> = None;
    for nyi in yi.saturating_sub(ky)..(yi + ky + 1).min(wind_speed.len()) {
        for nxi in xi.saturating_sub(kx)..(xi + kx + 1).min(wind_speed[nyi].len()) {
            let Some(w) = wind_speed[nyi][nxi] else { continue };
            if (x[nxi] - x[xi]).hypot(y[nyi] - y[yi]) <= radius_km {
                best = Some(best.map_or(w, |b: f32| b.max(w)));
            }
        }
    }
    best
}

/// Masks reflectivity cells at or below [`wind_clutter_cutoff_dbz`] whose
/// wind speed is at least [`wind_clutter_threshold_ms`]. `wind_speed` must
/// be the same grid as `reflectivity` (same file, same level), with `x`/`y`
/// its km coordinates. A cell with no wind of its own borrows the strongest
/// wind within `clutter_wind_search_km` when that's enabled; otherwise it's
/// left alone (no evidence either way). `z_km` is the plane's height, for
/// `clutter_max_height_km` — `None` (an altitude composite) skips that limit.
/// A shape mismatch, or `clutter_enabled = false`, returns 0.
pub fn qc_wind_clutter(
    reflectivity: &mut [Vec<Option<f32>>],
    wind_speed: &[Vec<Option<f32>>],
    x: &[f32],
    y: &[f32],
    z_km: Option<f32>,
    params: &QcParams,
) -> usize {
    if !params.clutter_enabled {
        return 0;
    }
    if let (Some(z), Some(max)) = (z_km, params.clutter_max_height_km) {
        if z > max {
            return 0;
        }
    }
    if reflectivity.len() != wind_speed.len()
        || reflectivity.len() != y.len()
        || reflectivity.iter().zip(wind_speed).any(|(r, w)| r.len() != w.len() || r.len() != x.len())
    {
        return 0;
    }
    let cutoff = wind_clutter_cutoff_dbz(reflectivity, params);
    let threshold = wind_clutter_threshold_ms(wind_speed, params);
    let search = params.clutter_wind_search_km;

    // Flag first, then mask — the neighborhood search reads `wind_speed`
    // only, so order doesn't matter for correctness, but this keeps the
    // shape of every other pass in this module.
    let mut flags = Vec::new();
    for (yi, row) in reflectivity.iter().enumerate() {
        for (xi, cell) in row.iter().enumerate() {
            let Some(dbz) = *cell else { continue };
            if dbz > cutoff {
                continue;
            }
            let ws = match wind_speed[yi][xi] {
                Some(w) => Some(w),
                None if search > 0.0 => max_wind_within(wind_speed, x, y, yi, xi, search),
                None => None,
            };
            if ws.is_some_and(|w| w >= threshold) {
                flags.push((yi, xi));
            }
        }
    }
    for &(yi, xi) in &flags {
        reflectivity[yi][xi] = None;
    }
    flags.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A flat 9x9 plane of `base`, `x`/`y` in 1 km steps centered on the
    /// middle cell — enough room for a 5x5 despike window and a few
    /// azimuthal rings without edge effects dominating.
    fn flat_plane(base: f32) -> (Vec<Vec<Option<f32>>>, Vec<f32>, Vec<f32>) {
        let coords: Vec<f32> = (0..9).map(|i| i as f32 - 4.0).collect();
        let data = vec![vec![Some(base); 9]; 9];
        (data, coords.clone(), coords)
    }

    #[test]
    fn despike_flags_and_masks_an_isolated_spike() {
        let (mut data, x, y) = flat_plane(10.0);
        data[4][4] = Some(500.0); // way off from its 5x5 neighborhood
        let report = qc_plane_xy(&mut data, &x, &y, &QcParams::default());
        assert_eq!(data[4][4], None);
        assert_eq!(report.cells_flagged_despike, 1);
    }

    #[test]
    fn despike_leaves_a_smooth_plane_untouched() {
        let (mut data, x, y) = flat_plane(10.0);
        let report = qc_plane_xy(&mut data, &x, &y, &QcParams::default());
        assert_eq!(report.cells_flagged_despike, 0);
        assert_eq!(report.cells_flagged_edge, 0);
        assert_eq!(report.cells_flagged_azimuthal, 0);
        assert!(data.iter().flatten().all(|v| v.is_some()));
    }

    #[test]
    fn edge_trim_flags_low_coverage_cells_regardless_of_value() {
        let (mut data, x, y) = flat_plane(10.0);
        // Isolate one cell: mask everything in its 5x5 neighborhood except
        // itself, leaving it with 0 valid neighbors (< min_coverage).
        for dy in -2i32..=2 {
            for dx in -2i32..=2 {
                if dx == 0 && dy == 0 {
                    continue;
                }
                data[(4 + dy) as usize][(4 + dx) as usize] = None;
            }
        }
        let report = qc_plane_xy(&mut data, &x, &y, &QcParams::default());
        assert_eq!(data[4][4], None);
        assert_eq!(report.cells_flagged_edge, 1);
        assert_eq!(report.cells_flagged_despike, 0, "an edge-trimmed cell shouldn't also count as a despike flag");
    }

    #[test]
    fn azimuthal_pass_flags_an_outlier_within_its_own_ring() {
        // A plane whose value is a genuine, smooth function of radius (so a
        // per-radius-bin median is meaningful), with one cell in the middle
        // ring nudged far off from its ring-mates.
        let coords: Vec<f32> = (0..21).map(|i| i as f32 - 10.0).collect();
        let mut data = vec![vec![None; 21]; 21];
        for (yi, &yv) in coords.iter().enumerate() {
            for (xi, &xv) in coords.iter().enumerate() {
                let r = (xv * xv + yv * yv).sqrt();
                data[yi][xi] = Some(50.0 - r); // smooth radial falloff
            }
        }
        // Ring around r ~= 6km: bump one cell there far above its neighbors.
        data[10][16] = Some(500.0); // (x=6, y=0) -> r=6
        let params = QcParams { min_neighbors: 4, ..Default::default() };
        let report = qc_plane_xy(&mut data, &coords, &coords, &params);
        assert_eq!(data[10][16], None);
        assert!(report.cells_flagged_azimuthal >= 1);
    }

    #[test]
    fn azimuthal_pass_does_not_erase_genuine_wavenumber_one_asymmetry() {
        // A real, smoothly-varying azimuthal asymmetry (wavenumber-1 —
        // stronger on one side of the storm) shouldn't itself look like an
        // outlier: every cell in a ring is a *gradual* function of azimuth,
        // never a lone spike relative to its own ring.
        let coords: Vec<f32> = (0..21).map(|i| i as f32 - 10.0).collect();
        let mut data = vec![vec![None; 21]; 21];
        for (yi, &yv) in coords.iter().enumerate() {
            for (xi, &xv) in coords.iter().enumerate() {
                let r = (xv * xv + yv * yv).sqrt();
                if r < 1e-3 {
                    data[yi][xi] = Some(50.0);
                    continue;
                }
                let az = yv.atan2(xv);
                data[yi][xi] = Some(50.0 - r + 5.0 * az.cos()); // smooth wavenumber-1 wobble
            }
        }
        let before = count_valid(&data);
        let params = QcParams { min_neighbors: 4, ..Default::default() };
        let report = qc_plane_xy(&mut data, &coords, &coords, &params);
        // A handful of cells near the aliasing-prone inner rings can
        // legitimately trip a coarse robust threshold; the check is that
        // this doesn't gut the field, not that it flags exactly zero.
        assert!(count_valid(&data) as f32 > before as f32 * 0.9, "azimuthal check over-flagged real asymmetry");
        let _ = report;
    }

    #[test]
    fn vertical_pass_flags_a_single_bad_level() {
        // 5 levels, all 10.0 at one (x,y) except level 2, which is a spike.
        let mut vol = vec![vec![vec![Some(10.0); 3]; 3]; 5];
        vol[2][1][1] = Some(500.0);
        let coords = vec![-1.0f32, 0.0, 1.0];
        let report = qc_volume(&mut vol, &coords, &coords, &QcParams::default());
        assert_eq!(vol[2][1][1], None);
        assert!(report.cells_flagged_vertical >= 1);
        // The same-level spatial pass would also have caught this cell
        // (it's a spatial spike too, at level 2) — either count landing on
        // it is fine, the important thing is it's gone.
        assert!(vol[0][1][1].is_some() && vol[4][1][1].is_some(), "other levels at the same (x,y) untouched");
    }

    #[test]
    fn cross_consistency_flags_a_mismatched_cell() {
        let primary_flat = vec![vec![Some(10.0); 4]; 4];
        let counterpart = vec![vec![Some(10.0); 4]; 4];
        let mut primary = primary_flat.clone();
        primary[1][1] = Some(200.0); // way off from its counterpart's 10.0
        let flagged = qc_cross_consistency(&mut primary, &counterpart, "reflectivity", &QcParams { min_neighbors: 4, ..Default::default() });
        assert_eq!(flagged, 1);
        assert_eq!(primary[1][1], None);
    }

    #[test]
    fn cross_consistency_tolerates_a_constant_offset_for_u_v() {
        // xy_rel's u = xy's u - 5.0 (storm motion) everywhere — a real,
        // uniform relationship, not an error, so nothing should be flagged.
        let counterpart = vec![vec![Some(10.0); 4]; 4];
        let mut primary = vec![vec![Some(15.0); 4]; 4];
        let flagged = qc_cross_consistency(&mut primary, &counterpart, "u", &QcParams { min_neighbors: 4, ..Default::default() });
        assert_eq!(flagged, 0);
        assert!(primary.iter().flatten().all(|v| v.is_some()));
    }

    #[test]
    fn cross_consistency_skips_radial_and_tangential_wind() {
        let counterpart = vec![vec![Some(10.0); 4]; 4];
        let mut primary = vec![vec![Some(10.0); 4]; 4];
        primary[0][0] = Some(9999.0);
        let flagged = qc_cross_consistency(&mut primary, &counterpart, "radial_wind", &QcParams::default());
        assert_eq!(flagged, 0);
        assert_eq!(primary[0][0], Some(9999.0), "skipped field must be left untouched");
    }

    #[test]
    fn qc_plane_vert_has_no_azimuthal_field() {
        let mut data = vec![vec![Some(10.0); 9]; 9];
        data[4][4] = Some(500.0);
        let report = qc_plane_vert(&mut data, &QcParams::default());
        assert_eq!(report.cells_flagged_azimuthal, 0);
        assert_eq!(data[4][4], None, "despiking still applies to vert profiles");
    }

    /// 10x10 reflectivity plane of `base` dBZ plus a matching wind plane of
    /// `wind` m/s.
    fn refl_and_wind(base: f32, wind: f32) -> (Vec<Vec<Option<f32>>>, Vec<Vec<Option<f32>>>) {
        (vec![vec![Some(base); 10]; 10], vec![vec![Some(wind); 10]; 10])
    }

    /// km coordinates for the 10x10 test grids — 2 km spacing.
    const XY: [f32; 10] = [0.0, 2.0, 4.0, 6.0, 8.0, 10.0, 12.0, 14.0, 16.0, 18.0];

    #[test]
    fn wind_clutter_cutoff_scales_with_storm_strength() {
        let p = QcParams::default();
        let (weak, _) = refl_and_wind(12.0, 0.0);
        let (mid, _) = refl_and_wind(22.5, 0.0);
        let (strong, _) = refl_and_wind(40.0, 0.0);
        assert_eq!(wind_clutter_cutoff_dbz(&weak, &p), 2.0);
        assert!((wind_clutter_cutoff_dbz(&mid, &p) - 3.5).abs() < 1e-4);
        assert_eq!(wind_clutter_cutoff_dbz(&strong, &p), 5.0);
        // No genuine echoes at all -> low cutoff.
        let (empty, _) = refl_and_wind(1.0, 0.0);
        assert_eq!(wind_clutter_cutoff_dbz(&empty, &p), 2.0);
    }

    #[test]
    fn wind_clutter_cutoff_ignores_the_phantoms_themselves() {
        // Half the plane is 0-dBZ phantom; the cutoff should still read the
        // strong storm from the other half rather than a diluted median.
        let p = QcParams::default();
        let (mut refl, _) = refl_and_wind(35.0, 0.0);
        for row in refl.iter_mut().take(5) {
            row.fill(Some(0.5));
        }
        assert_eq!(wind_clutter_cutoff_dbz(&refl, &p), 5.0);
    }

    #[test]
    fn wind_clutter_masks_weak_echo_only_under_strong_wind() {
        let p = QcParams::default();
        let (mut refl, mut wind) = refl_and_wind(35.0, 10.0);
        refl[2][2] = Some(1.0); // weak, strong wind -> phantom
        wind[2][2] = Some(40.0);
        refl[3][3] = Some(1.0); // weak, light wind -> kept
        refl[4][4] = Some(30.0); // strong wind but real echo -> kept
        wind[4][4] = Some(40.0);
        refl[5][5] = Some(1.0); // weak, no wind on record -> kept
        wind[5][5] = None;
        assert_eq!(qc_wind_clutter(&mut refl, &wind, &XY, &XY, None, &p), 1);
        assert_eq!(refl[2][2], None);
        assert_eq!(refl[3][3], Some(1.0));
        assert_eq!(refl[4][4], Some(30.0));
        assert_eq!(refl[5][5], Some(1.0));
    }

    #[test]
    fn wind_clutter_keeps_4dbz_in_a_weak_storm_but_not_a_strong_one() {
        let p = QcParams::default();
        let (mut weak, wind) = refl_and_wind(12.0, 40.0);
        weak[0][0] = Some(4.0);
        qc_wind_clutter(&mut weak, &wind, &XY, &XY, None, &p);
        assert_eq!(weak[0][0], Some(4.0)); // cutoff 2 dBZ

        let (mut strong, wind) = refl_and_wind(35.0, 40.0);
        strong[0][0] = Some(4.0);
        qc_wind_clutter(&mut strong, &wind, &XY, &XY, None, &p);
        assert_eq!(strong[0][0], None); // cutoff 5 dBZ
    }

    #[test]
    fn wind_clutter_skips_mismatched_grids() {
        let p = QcParams::default();
        let (mut refl, _) = refl_and_wind(1.0, 0.0);
        let wind = vec![vec![Some(40.0); 9]; 10];
        assert_eq!(qc_wind_clutter(&mut refl, &wind, &XY, &XY, None, &p), 0);
    }

    #[test]
    fn wind_clutter_search_borrows_nearby_wind_for_cells_without_any() {
        let (mut refl, mut wind) = refl_and_wind(35.0, 10.0);
        refl[5][5] = Some(0.5);
        wind[5][5] = None;
        wind[5][7] = Some(40.0); // 4 km away
        let off = QcParams::default();
        assert_eq!(qc_wind_clutter(&mut refl.clone(), &wind, &XY, &XY, None, &off), 0);
        let near = QcParams { clutter_wind_search_km: 3.0, ..Default::default() };
        assert_eq!(qc_wind_clutter(&mut refl.clone(), &wind, &XY, &XY, None, &near), 0);
        let far = QcParams { clutter_wind_search_km: 4.0, ..Default::default() };
        assert_eq!(qc_wind_clutter(&mut refl, &wind, &XY, &XY, None, &far), 1);
        assert_eq!(refl[5][5], None);
    }

    #[test]
    fn wind_clutter_threshold_can_follow_the_planes_peak_wind() {
        // Peak 30 m/s; a 22 m/s cell isn't "strong" at the fixed 25 m/s
        // threshold, but is at 70% of peak (21 m/s).
        let (mut refl, mut wind) = refl_and_wind(35.0, 10.0);
        wind[0][0] = Some(30.0);
        refl[3][3] = Some(1.0);
        wind[3][3] = Some(22.0);
        assert_eq!(wind_clutter_threshold_ms(&wind, &QcParams::default()), 25.0);
        let frac = QcParams { clutter_wind_frac_of_peak: 0.7, ..Default::default() };
        assert!((wind_clutter_threshold_ms(&wind, &frac) - 21.0).abs() < 1e-4);
        assert_eq!(qc_wind_clutter(&mut refl, &wind, &XY, &XY, None, &frac), 1);
    }

    #[test]
    fn wind_clutter_respects_max_height_and_enabled() {
        let (mut refl, mut wind) = refl_and_wind(35.0, 10.0);
        refl[2][2] = Some(1.0);
        wind[2][2] = Some(40.0);
        let capped = QcParams { clutter_max_height_km: Some(6.0), ..Default::default() };
        assert_eq!(qc_wind_clutter(&mut refl.clone(), &wind, &XY, &XY, Some(8.0), &capped), 0);
        assert_eq!(qc_wind_clutter(&mut refl.clone(), &wind, &XY, &XY, None, &capped), 1);
        let off = QcParams { clutter_enabled: false, ..Default::default() };
        assert_eq!(qc_wind_clutter(&mut refl.clone(), &wind, &XY, &XY, Some(2.0), &off), 0);
        assert_eq!(qc_wind_clutter(&mut refl, &wind, &XY, &XY, Some(2.0), &capped), 1);
    }
}
