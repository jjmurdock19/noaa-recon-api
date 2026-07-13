//! Band / cmap / product catalog constants — the functional bits of
//! `app/services/goes.py` + the `VALID_*` sets in `app/routers/satellite.py`.
//! Pure data; presentation text (descriptions) stays in the server router.

use crate::colormap::{LUT_NAMES, STOPS_NAMES};

/// Bands offered as standalone products (`VALID_BANDS`).
pub const VALID_BANDS: [i64; 6] = [2, 3, 5, 7, 9, 13];
/// Composite products (`VALID_PRODUCTS`).
pub const VALID_PRODUCTS: [&str; 2] = ["geocolor", "sandwich"];
/// Bands that report reflectance, not brightness temperature (`REFLECTANCE_BANDS`).
pub const REFLECTANCE_BANDS: [i64; 6] = [1, 2, 3, 4, 5, 6];
/// Reflectance-band cmap sentinels (`REFLECTANCE_CMAPS`).
pub const REFLECTANCE_CMAPS: [&str; 3] = ["abi2", "abi3", "abi5"];

pub fn is_reflectance_band(band: i64) -> bool {
    REFLECTANCE_BANDS.contains(&band)
}

pub fn is_reflectance_cmap(cmap: &str) -> bool {
    REFLECTANCE_CMAPS.contains(&cmap)
}

/// `DEFAULT_CMAP_BY_BAND`.
pub fn default_cmap_by_band(band: i64) -> Option<&'static str> {
    Some(match band {
        13 => "abi13",
        9 => "abi9",
        7 => "abi7",
        5 => "abi5",
        3 => "abi3",
        2 => "abi2",
        _ => return None,
    })
}

/// `NATIVE_GSD_KM` — native ground sample distance (km/px) per band.
pub fn native_gsd_km(band: i64) -> Option<f64> {
    Some(match band {
        1 => 1.0,
        2 => 0.5,
        3 => 1.0,
        5 => 1.0,
        7 => 2.0,
        9 => 2.0,
        13 => 2.0,
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
