//! Band / cmap / product catalog constants — the functional bits of
//! `app/services/goes.py` + the `VALID_*` sets in `app/routers/satellite.py`.
//! Pure data; presentation text (descriptions) stays in the server router.
//!
//! Band wavelengths/nicknames/native resolution below are sourced from
//! NOAA/NESDIS's official ABI Bands Quick Guides
//! (<https://www.goes-r.gov/mission/ABI-bands-quick-info.html>). Every ABI
//! band (1-16) is renderable; the six reflective/near-IR bands (1-6) share
//! the same gamma-stretched reflectance ramp (`reflectance_gray` —
//! `colorize()` routes on `is_reflectance_band`, not on which specific
//! `abiN` cmap sentinel was requested, so 1/4/6 render identically to
//! 2/3/5's already-verified path). The ten IR bands split into three
//! default-enhancement families by what they physically measure, reusing
//! this project's existing verified exact-stops/LUT tables rather than
//! fabricating new per-band palettes from un-OCR-able scanned quick-guide
//! PDFs: Band 7 keeps its own shortwave/"fire temperature" stops (`abi7`);
//! the water-vapor trio (8 upper-level / 9 mid-level / 10 lower-level)
//! shares `abi9`'s stops — NOAA's own WV products use near-identical
//! enhancements across that trio; the longwave-window family (11 cloud-top
//! phase, 12 ozone, 13 clean window, 14 window, 15 dirty window, 16 CO2)
//! defaults to `abi13`'s stops except 13 itself, which is `ir4` (the
//! ColorBrewer-Spectral rainbow LUT already in this codebase) — all of
//! these observe a similar cloud-top/surface brightness-temperature range
//! and NOAA's quick guides show a comparable rainbow-window convention
//! across them (confirmed against the real Band 16 quick guide's colorbar).
//! `?cmap=` still accepts any cmap regardless of band if a caller wants a
//! different look.

use crate::colormap::{LUT_NAMES, STOPS_NAMES};

/// Every ABI band (`VALID_BANDS`) — 1-6 reflective/near-IR, 7-16 IR.
pub const VALID_BANDS: [i64; 16] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
/// Composite products (`VALID_PRODUCTS`).
pub const VALID_PRODUCTS: [&str; 2] = ["geocolor", "sandwich"];
/// Bands that report reflectance, not brightness temperature (`REFLECTANCE_BANDS`).
pub const REFLECTANCE_BANDS: [i64; 6] = [1, 2, 3, 4, 5, 6];
/// Reflectance-band cmap sentinels (`REFLECTANCE_CMAPS`).
pub const REFLECTANCE_CMAPS: [&str; 6] = ["abi1", "abi2", "abi3", "abi4", "abi5", "abi6"];

pub fn is_reflectance_band(band: i64) -> bool {
    REFLECTANCE_BANDS.contains(&band)
}

pub fn is_reflectance_cmap(cmap: &str) -> bool {
    REFLECTANCE_CMAPS.contains(&cmap)
}

/// `DEFAULT_CMAP_BY_BAND` — see the module doc for why 8/10 share `abi9` and
/// 11/12/14/15/16 share `abi13`/`ir4` rather than each having a bespoke table.
pub fn default_cmap_by_band(band: i64) -> Option<&'static str> {
    Some(match band {
        1 => "abi1",
        2 => "abi2",
        3 => "abi3",
        4 => "abi4",
        5 => "abi5",
        6 => "abi6",
        7 => "abi7",
        8 => "abi9",  // upper-level WV — shares the WV trio's enhancement
        9 => "abi9",
        10 => "abi9", // lower-level WV — shares the WV trio's enhancement
        11 => "ir4",  // cloud-top phase — longwave-window family default (see module doc)
        12 => "ir4",  // ozone
        13 => "abi13", // clean IR window — its own dedicated, verified stops table
        14 => "ir4",  // IR longwave window
        15 => "ir4",  // "dirty" longwave window
        16 => "ir4",  // CO2 longwave IR
        _ => return None,
    })
}

/// `NATIVE_GSD_KM` — native ground sample distance (km/px) per band. Bands
/// 4/6 (near-IR) and every IR band (7-16) are 2km; band 1 is 1km, band 2 is
/// 0.5km (the sharpest ABI band), bands 3/5 are 1km.
pub fn native_gsd_km(band: i64) -> Option<f64> {
    Some(match band {
        1 => 1.0,
        2 => 0.5,
        3 => 1.0,
        4 => 2.0,
        5 => 1.0,
        6 => 2.0,
        7..=16 => 2.0,
        _ => return None,
    })
}

/// Every cmap accepted by `/tile` (`VALID_CMAPS`): LUTs + exact-stops +
/// reflectance sentinels + "default".
pub fn valid_cmaps() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = Vec::new();
    v.extend_from_slice(&LUT_NAMES);
    v.extend_from_slice(&STOPS_NAMES);
    v.extend_from_slice(&REFLECTANCE_CMAPS);
    v.push("default");
    v
}

pub fn is_valid_cmap(cmap: &str) -> bool {
    cmap == "default"
        || LUT_NAMES.contains(&cmap)
        || STOPS_NAMES.contains(&cmap)
        || REFLECTANCE_CMAPS.contains(&cmap)
}

pub fn is_valid_band(band: i64) -> bool {
    VALID_BANDS.contains(&band)
}

pub fn is_valid_product(product: &str) -> bool {
    VALID_PRODUCTS.contains(&product)
}
