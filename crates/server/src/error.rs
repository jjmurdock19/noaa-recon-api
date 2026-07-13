//! HTTP error type — the axum equivalent of FastAPI's `HTTPException`.
//!
//! FastAPI serializes `HTTPException(status, detail)` as `{"detail": <detail>}`
//! with the given status code. `ApiError` reproduces that body exactly so error
//! responses match the Python version byte-for-byte.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub detail: String,
}

impl ApiError {
    pub fn new(status: StatusCode, detail: impl Into<String>) -> Self {
        Self { status, detail: detail.into() }
    }
    pub fn not_found(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, detail)
    }
    pub fn bad_request(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, detail)
    }
    pub fn conflict(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, detail)
    }
    pub fn bad_gateway(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, detail)
    }
    pub fn not_implemented(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_IMPLEMENTED, detail)
    }
    pub fn internal(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, detail)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "detail": self.detail }))).into_response()
    }
}

/// Any unhandled DB error becomes a 500 (mirrors FastAPI's default handling of
/// an uncaught exception in a route).
impl From<rusqlite::Error> for ApiError {
    fn from(e: rusqlite::Error) -> Self {
        ApiError::internal(format!("database error: {e}"))
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError::internal(e.to_string())
    }
}

pub type ApiResult<T> = Result<T, ApiError>;
