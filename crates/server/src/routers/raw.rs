//! Port of `app/routers/raw.py` — raw netCDF passthrough. STUB (501) in both
//! versions; the planned subsetting endpoint is a follow-up phase.

use axum::routing::get;
use axum::Router;

use crate::error::ApiError;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/raw/netcdf", get(get_raw_netcdf))
}

async fn get_raw_netcdf() -> ApiError {
    ApiError::not_implemented(
        "Not implemented yet. Raw netCDF passthrough is a follow-up phase — see \
         app/routers/raw.py for the planned request/response shape.",
    )
}
