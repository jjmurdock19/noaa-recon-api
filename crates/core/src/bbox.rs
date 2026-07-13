//! Bounding-box request validation/clamping — port of `resolve_bbox_request`
//! and its constants from `app/services/goes.py`. Pure math (WASM-safe).

use crate::catalog::native_gsd_km;

pub const MIN_BBOX_WIDTH_KM: f64 = 10.0;
pub const MAX_BBOX_WIDTH_KM: f64 = 8000.0;
pub const KM_PER_DEG_LAT: f64 = 111.32;
pub const MIN_OUT_SIZE: i64 = 64;
pub const MAX_OUT_SIZE: i64 = 4096;

/// The geographic box (lat_S, lat_N, lon_W, lon_E) for a center+width request —
/// shared by the server's netCDF crop-locate step and the render step so both
/// agree. Port of the top of `render_bbox_to_png`.
pub fn bbox_bounds(center_lat: f64, center_lon: f64, width_km: f64) -> (f64, f64, f64, f64) {
    let half_km = width_km / 2.0;
    let lat_half = half_km / KM_PER_DEG_LAT;
    let lon_half = half_km / (KM_PER_DEG_LAT * center_lat.to_radians().cos().max(0.01));
    (
        center_lat - lat_half,
        center_lat + lat_half,
        center_lon - lon_half,
        center_lon + lon_half,
    )
}

/// Output canvas size for a bbox render: `clip(round(width/res), 64, 4096)`.
pub fn bbox_out_size(width_km: f64, resolution_km: f64) -> usize {
    (width_km / resolution_km).round().clamp(MIN_OUT_SIZE as f64, MAX_OUT_SIZE as f64) as usize
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BBoxRequest {
    pub center_lat: f64,
    pub center_lon: f64,
    pub width_km: f64,
    pub resolution_km: f64,
}

/// Validate and clamp a bbox request. Returns `Err(message)` on out-of-range
/// center coordinates (the Python raises `ValueError` -> HTTP 400).
pub fn resolve_bbox_request(
    center_lat: f64,
    center_lon: f64,
    width_km: f64,
    resolution_km: Option<f64>,
    band: i64,
) -> Result<BBoxRequest, String> {
    if !(-90.0..=90.0).contains(&center_lat) {
        return Err(format!("center latitude {center_lat} out of range [-90, 90]"));
    }
    if !(-180.0..=180.0).contains(&center_lon) {
        return Err(format!("center longitude {center_lon} out of range [-180, 180]"));
    }
    let width_km = width_km.clamp(MIN_BBOX_WIDTH_KM, MAX_BBOX_WIDTH_KM);

    let native = native_gsd_km(band).unwrap_or(2.0);
    // Can't resolve finer than native pixel size.
    let resolution_km = match resolution_km {
        None => native,
        Some(r) => r.max(native),
    };

    Ok(BBoxRequest { center_lat, center_lon, width_km, resolution_km })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_width_and_resolution() {
        // width below min clamps up; resolution finer than band-2 native (0.5) clamps up.
        let b = resolve_bbox_request(25.0, -80.0, 1.0, Some(0.1), 2).unwrap();
        assert_eq!(b.width_km, MIN_BBOX_WIDTH_KM);
        assert_eq!(b.resolution_km, 0.5);
    }

    #[test]
    fn rejects_out_of_range_center() {
        assert!(resolve_bbox_request(91.0, 0.0, 100.0, None, 13).is_err());
        assert!(resolve_bbox_request(0.0, 200.0, 100.0, None, 13).is_err());
    }

    #[test]
    fn default_resolution_is_native() {
        let b = resolve_bbox_request(0.0, 0.0, 500.0, None, 13).unwrap();
        assert_eq!(b.resolution_km, 2.0);
    }
}
