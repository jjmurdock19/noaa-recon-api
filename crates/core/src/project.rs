//! ABI fixed-grid geometry — port of `abi_to_latlon` (PUG Vol 5 §4.2) and
//! `_mercator_y` from `app/services/goes.py`. Pure scalar math (WASM-safe); the
//! render loop calls these per source pixel instead of building numpy meshgrids.

/// Convert one ABI fixed-grid (x_rad, y_rad) sample to geographic (lon, lat) in
/// degrees. Returns `None` for an off-disk sample (no ray/ellipsoid
/// intersection), matching the Python's NaN.
pub fn abi_to_latlon(
    x_rad: f64,
    y_rad: f64,
    sat_lon_deg: f64,
    h: f64,
    r_eq: f64,
    r_pol: f64,
) -> Option<(f64, f64)> {
    let big_h = r_eq + h;
    let lam0 = sat_lon_deg.to_radians();

    let (sx, cx) = (x_rad.sin(), x_rad.cos());
    let (sy, cy) = (y_rad.sin(), y_rad.cos());

    let a1 = sx * sx + cx * cx * (cy * cy + (r_eq / r_pol).powi(2) * sy * sy);
    let b1 = -2.0 * big_h * cx * cy;
    let c1 = big_h * big_h - r_eq * r_eq;

    let disc = b1 * b1 - 4.0 * a1 * c1;
    if disc < 0.0 {
        return None;
    }
    let rs = (-b1 - disc.sqrt()) / (2.0 * a1);

    // PUG Vol 5 §4.2: Sx = H - rs*cos(x)*cos(y). Getting Sx's sign wrong rotates
    // every longitude by 180°.
    let sx_ = big_h - rs * cx * cy;
    let sy_ = -rs * sx;
    let sz_ = rs * cx * sy;

    let lat_rad = ((r_eq / r_pol).powi(2) * sz_ / (sx_ * sx_ + sy_ * sy_).sqrt()).atan();
    let lon_rad = lam0 - sy_.atan2(sx_);

    Some((lon_rad.to_degrees(), lat_rad.to_degrees()))
}

/// Web-Mercator Y for a latitude in degrees (`_mercator_y`). Output rows are
/// spaced linearly in this, not in latitude, so Leaflet's linear image stretch
/// reproduces the correct geography.
pub fn mercator_y(lat_deg: f64) -> f64 {
    let lat_rad = lat_deg.to_radians();
    (std::f64::consts::FRAC_PI_4 + lat_rad / 2.0).tan().ln()
}

#[cfg(test)]
mod tests {
    use super::*;

    // GOES-East nadir: x=0,y=0 should map to (sat_lon, 0).
    #[test]
    fn nadir_maps_to_subsatellite_point() {
        let sat_lon = -75.0;
        let (lon, lat) = abi_to_latlon(0.0, 0.0, sat_lon, 35786023.0, 6378137.0, 6356752.31414).unwrap();
        assert!((lon - sat_lon).abs() < 1e-6, "lon={lon}");
        assert!(lat.abs() < 1e-6, "lat={lat}");
    }

    #[test]
    fn far_off_disk_returns_none() {
        // A large scan angle points past the limb — no intersection.
        assert!(abi_to_latlon(0.3, 0.3, -75.0, 35786023.0, 6378137.0, 6356752.31414).is_none());
    }

    #[test]
    fn mercator_y_zero_at_equator() {
        assert!(mercator_y(0.0).abs() < 1e-12);
    }
}
