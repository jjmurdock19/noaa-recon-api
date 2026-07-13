//! Port of `app/routers/tdr.py` — Tail Doppler Radar. Still a STUB upstream
//! (501): TDR mission indexing/parsing is a follow-up phase in both versions.

use axum::routing::get;
use axum::Router;

use crate::error::ApiError;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/tdr/missions", get(list_missions))
        .route("/tdr/sweep", get(get_sweep))
}

async fn list_missions() -> ApiError {
    ApiError::not_implemented(
        "Not implemented yet. TDR mission indexing (crawling seb.omao.noaa.gov, \
         extracting .tar.gz bundles, parsing raw netCDF) is a follow-up phase.",
    )
}

async fn get_sweep() -> ApiError {
    ApiError::not_implemented(
        "Not implemented yet. TDR sweep rendering is a follow-up phase — see \
         app/services/tdr.py and the README's TDR section.",
    )
}
