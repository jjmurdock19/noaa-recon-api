//! GOES colormaps + brightness-temperature/reflectance ramps.
//!
//! Port of the colortable half of `app/services/goes.py` (the numpy-free parts).
//! WASM-safe: pure arithmetic, no I/O. This is the single source of truth the
//! render pipeline and the `/colortable*` endpoints both draw from — and exactly
//! the kind of code the browser build will run itself.

pub type Rgb = [u8; 3];

// ── Temperature <-> colormap index ──────────────────────────────────────────
pub const TEMP_MIN_K: f64 = 160.0;
pub const TEMP_MAX_K: f64 = 315.0;

/// index (0..=255) -> temperature K. `_i2t` in Python.
pub fn i2t(idx: usize) -> f64 {
    TEMP_MAX_K - (idx as f64 / 255.0) * (TEMP_MAX_K - TEMP_MIN_K)
}

/// temperature K -> index (0..=255), clamped. `_t2i` in Python.
pub fn t2i(temp_k: f64) -> u8 {
    let clamped = temp_k.clamp(TEMP_MIN_K, TEMP_MAX_K);
    ((TEMP_MAX_K - clamped) / (TEMP_MAX_K - TEMP_MIN_K) * 255.0).round() as u8
}

/// `_lerp`: np.interp over [t0,t1]->[v0,v1] (clamped to the endpoints), then
/// clamped to 0..=255 and rounded to an int channel value.
fn lerp(t: f64, t0: f64, t1: f64, v0: f64, v1: f64) -> u8 {
    let interp = if t <= t0 {
        v0
    } else if t >= t1 {
        v1
    } else {
        v0 + (v1 - v0) * (t - t0) / (t1 - t0)
    };
    interp.clamp(0.0, 255.0).round() as u8
}

/// `_interp_stops`: ascending (temp_C, rgb) stops, linear interp between
/// neighbors, clamped to the end colors outside the range.
fn interp_stops(t_c: f64, stops: &[(f64, Rgb)]) -> Rgb {
    if t_c <= stops[0].0 {
        return stops[0].1;
    }
    if t_c >= stops[stops.len() - 1].0 {
        return stops[stops.len() - 1].1;
    }
    for w in stops.windows(2) {
        let (t0, c0) = w[0];
        let (t1, c1) = w[1];
        if t0 <= t_c && t_c <= t1 {
            let frac = if t1 != t0 { (t_c - t0) / (t1 - t0) } else { 0.0 };
            return [
                (c0[0] as f64 + (c1[0] as f64 - c0[0] as f64) * frac).round() as u8,
                (c0[1] as f64 + (c1[1] as f64 - c0[1] as f64) * frac).round() as u8,
                (c0[2] as f64 + (c1[2] as f64 - c0[2] as f64) * frac).round() as u8,
            ];
        }
    }
    stops[stops.len() - 1].1
}

// ── LUT-based enhancements (input: temperature K) ───────────────────────────
fn bd(t: f64) -> Rgb {
    if t >= 241.0 {
        let v = lerp(t, 241.0, TEMP_MAX_K, 200.0, 0.0);
        [v, v, v]
    } else if t >= 220.0 {
        let v = lerp(t, 220.0, 241.0, 255.0, 200.0);
        [v, v, v]
    } else if t >= 210.0 {
        [0, lerp(t, 210.0, 220.0, 30.0, 140.0), 255]
    } else if t >= 200.0 {
        [0, 0, lerp(t, 200.0, 210.0, 180.0, 255.0)]
    } else if t >= 190.0 {
        [lerp(t, 190.0, 200.0, 150.0, 0.0), 0, 210]
    } else {
        [lerp(t, TEMP_MIN_K, 190.0, 255.0, 150.0), 0, lerp(t, TEMP_MIN_K, 190.0, 0.0, 210.0)]
    }
}

fn enhanced(t: f64) -> Rgb {
    if t >= 260.0 {
        let v = lerp(t, 260.0, TEMP_MAX_K, 160.0, 20.0);
        [v, v, v]
    } else if t >= 230.0 {
        let v = lerp(t, 230.0, 260.0, 255.0, 160.0);
        [v, v, v]
    } else if t >= 200.0 {
        [lerp(t, 200.0, 230.0, 0.0, 255.0), lerp(t, 200.0, 230.0, 0.0, 255.0), 255]
    } else {
        [lerp(t, TEMP_MIN_K, 200.0, 255.0, 0.0), 0, lerp(t, TEMP_MIN_K, 200.0, 0.0, 255.0)]
    }
}

fn nrl(t: f64) -> Rgb {
    if t >= 273.0 {
        let v = lerp(t, 273.0, TEMP_MAX_K, 80.0, 5.0);
        [v, v, v]
    } else if t >= 253.0 {
        let v = lerp(t, 253.0, 273.0, 130.0, 80.0);
        [v, v, v]
    } else if t >= 233.0 {
        let v = lerp(t, 233.0, 253.0, 255.0, 130.0);
        [v, v, v]
    } else if t >= 220.0 {
        [255, 255, lerp(t, 220.0, 233.0, 0.0, 255.0)]
    } else if t >= 210.0 {
        [lerp(t, 210.0, 220.0, 0.0, 255.0), 255, lerp(t, 210.0, 220.0, 255.0, 0.0)]
    } else if t >= 200.0 {
        [0, lerp(t, 200.0, 210.0, 80.0, 255.0), 255]
    } else if t >= 185.0 {
        [lerp(t, 185.0, 200.0, 200.0, 0.0), 0, 255]
    } else {
        [255, lerp(t, TEMP_MIN_K, 185.0, 100.0, 0.0), lerp(t, TEMP_MIN_K, 185.0, 0.0, 200.0)]
    }
}

fn grayscale(t: f64) -> Rgb {
    let v = lerp(t, TEMP_MIN_K, TEMP_MAX_K, 255.0, 0.0);
    [v, v, v]
}

// GOES IR4 (satpy colorized_ir_clouds, ColorBrewer Spectral-11).
const SPECTRAL_11: [Rgb; 11] = [
    [158, 1, 66], [213, 62, 79], [244, 109, 67], [253, 174, 97], [254, 224, 139],
    [255, 255, 191], [230, 245, 152], [171, 221, 164], [102, 194, 165], [50, 136, 189], [94, 79, 162],
];
const IR4_GREY_MIN_K: f64 = 253.15;
const IR4_GREY_MAX_K: f64 = 303.15;
const IR4_COLOR_MIN_K: f64 = 193.15;

fn spectral_interp(frac: f64) -> Rgb {
    let frac = frac.clamp(0.0, 1.0);
    let pos = frac * (SPECTRAL_11.len() - 1) as f64;
    let i0 = pos.floor() as usize;
    let i1 = (i0 + 1).min(SPECTRAL_11.len() - 1);
    let t = pos - i0 as f64;
    let (c0, c1) = (SPECTRAL_11[i0], SPECTRAL_11[i1]);
    [
        (c0[0] as f64 + (c1[0] as f64 - c0[0] as f64) * t).round() as u8,
        (c0[1] as f64 + (c1[1] as f64 - c0[1] as f64) * t).round() as u8,
        (c0[2] as f64 + (c1[2] as f64 - c0[2] as f64) * t).round() as u8,
    ]
}

fn goes_ir4(t: f64) -> Rgb {
    if t >= IR4_GREY_MAX_K {
        return [0, 0, 0];
    }
    if t >= IR4_GREY_MIN_K {
        let v = lerp(t, IR4_GREY_MIN_K, IR4_GREY_MAX_K, 255.0, 0.0);
        return [v, v, v];
    }
    let frac = (t - IR4_COLOR_MIN_K) / (IR4_GREY_MIN_K - IR4_COLOR_MIN_K);
    spectral_interp(frac)
}

/// The LUT-based cmaps: build a 256-entry table (`_build_lut`), one row per
/// index via `i2t`. Returns None for a name that isn't LUT-based.
pub fn build_lut(name: &str) -> Option<[Rgb; 256]> {
    let f: fn(f64) -> Rgb = match name {
        "bd" => bd,
        "enhanced" => enhanced,
        "nrl" => nrl,
        "grayscale" => grayscale,
        "ir4" => goes_ir4,
        _ => return None,
    };
    let mut lut = [[0u8; 3]; 256];
    for (i, row) in lut.iter_mut().enumerate() {
        *row = f(i2t(i));
    }
    Some(lut)
}

pub const LUT_NAMES: [&str; 5] = ["bd", "enhanced", "nrl", "grayscale", "ir4"];

// ── Exact-stops enhancements (input: temperature K) ─────────────────────────
const ABI13_STOPS: [(f64, Rgb); 13] = [
    (-110.0, [255, 255, 255]), (-80.0, [0, 0, 0]), (-75.0, [51, 0, 0]), (-65.0, [255, 69, 0]),
    (-59.0, [173, 255, 47]), (-50.0, [0, 255, 0]), (-40.0, [0, 0, 128]), (-32.0, [0, 255, 255]),
    (-31.0, [204, 204, 204]), (-20.0, [153, 153, 153]), (6.0, [102, 102, 102]), (31.0, [51, 51, 51]),
    (57.0, [0, 0, 0]),
];
const ABI9_STOPS: [(f64, Rgb); 11] = [
    (-93.0, [0, 255, 255]), (-75.0, [60, 179, 113]), (-54.0, [120, 171, 120]), (-42.0, [255, 255, 255]),
    (-30.0, [153, 153, 204]), (-24.0, [0, 0, 128]), (-18.0, [34, 34, 59]), (-12.0, [255, 255, 0]),
    (-5.0, [255, 127, 0]), (2.0, [255, 0, 0]), (7.0, [0, 0, 0]),
];
const ABI7_STOPS: [(f64, Rgb); 8] = [
    (-90.0, [255, 255, 255]), (-60.0, [0, 0, 0]), (-20.0, [90, 90, 90]), (0.0, [150, 150, 150]),
    (30.0, [210, 210, 210]), (57.0, [255, 255, 255]), (90.0, [255, 255, 0]), (130.0, [255, 0, 0]),
];

/// The exact-stops cmaps (evaluated per-pixel, not quantized through a LUT).
pub fn stops_by_cmap(name: &str) -> Option<&'static [(f64, Rgb)]> {
    match name {
        "abi13" => Some(&ABI13_STOPS),
        "abi9" => Some(&ABI9_STOPS),
        "abi7" => Some(&ABI7_STOPS),
        _ => None,
    }
}

pub const STOPS_NAMES: [&str; 3] = ["abi13", "abi9", "abi7"];

/// Colorize one brightness temperature (K) with an exact-stops cmap.
pub fn colorize_temp_exact(name: &str, temp_k: f64) -> Option<Rgb> {
    stops_by_cmap(name).map(|stops| interp_stops(temp_k - 273.15, stops))
}

/// Public wrapper over `interp_stops` for the render pipeline (temp in °C).
pub fn interp_stops_public(t_c: f64, stops: &[(f64, Rgb)]) -> Rgb {
    interp_stops(t_c, stops)
}

/// `_reflectance_gray`: reflectance factor -> 0..=255 grayscale value with a
/// mild gamma stretch (gamma 1.5).
pub fn reflectance_gray(refl: f64, gamma: f64) -> u8 {
    (refl.clamp(0.0, 1.0).powf(1.0 / gamma) * 255.0).round().clamp(0.0, 255.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_index_roundtrip_endpoints() {
        assert_eq!(t2i(TEMP_MAX_K), 0);
        assert_eq!(t2i(TEMP_MIN_K), 255);
        // clamps outside range
        assert_eq!(t2i(400.0), 0);
        assert_eq!(t2i(100.0), 255);
    }

    #[test]
    fn abi13_exact_stops_match_python() {
        // -110C -> white, -80C -> black, +57C -> black (endpoints of _ABI13_STOPS)
        assert_eq!(colorize_temp_exact("abi13", -110.0 + 273.15).unwrap(), [255, 255, 255]);
        assert_eq!(colorize_temp_exact("abi13", -80.0 + 273.15).unwrap(), [0, 0, 0]);
        // midpoint between -80(black) and -75(51,0,0) at -77.5 -> ~ (25.5,0,0)
        let mid = colorize_temp_exact("abi13", -77.5 + 273.15).unwrap();
        assert_eq!(mid, [26, 0, 0]);
    }

    #[test]
    fn lut_builds_256_rows() {
        let lut = build_lut("bd").unwrap();
        assert_eq!(lut.len(), 256);
        assert!(build_lut("not-a-lut").is_none());
    }

    #[test]
    fn reflectance_gray_endpoints() {
        assert_eq!(reflectance_gray(0.0, 1.5), 0);
        assert_eq!(reflectance_gray(1.0, 1.5), 255);
    }
}
