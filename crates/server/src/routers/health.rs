//! Port of `app/routers/health.py`.
//!
//! ```python
//! @router.get("/health")
//! async def health():
//!     return {"status": "ok"}
//! ```

use axum::{routing::get, Json, Router};
use serde_json::{json, Value};

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/health", get(health))
}

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}
