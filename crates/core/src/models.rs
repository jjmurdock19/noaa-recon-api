//! Shared response models — port of `app/models.py` (pydantic -> serde).
//!
//! Parity note: pydantic (with FastAPI's default `response_model_exclude_none=False`)
//! emits `None` fields as JSON `null`. serde does the same by default, so we
//! deliberately DO NOT add `skip_serializing_if = "Option::is_none"` — the wire
//! output must match the Python version byte-for-byte for the benchmark A/B.

// Not all fields/variants are referenced yet — the routers that build these
// land later. Silence dead-code noise until then.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TileState {
    Ready,
    Generating,
    Error,
    Idle,
}

/// Port of `TileStatus`. `status` is the only required field (a Literal in
/// pydantic -> an enum here); everything else is optional.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TileStatus {
    pub status: TileState,
    pub key: Option<String>,
    pub png_url: Option<String>,
    pub bounds: Option<Vec<Vec<f64>>>,
    pub band: Option<i64>,
    pub cmap: Option<String>,
    pub product: Option<String>,
    pub satellite: Option<String>,
    pub sat_lon: Option<f64>,
    pub scan_start: Option<String>,
    pub elapsed: Option<i64>,
    pub message: Option<String>,
    pub center: Option<Vec<f64>>,
    pub width_km: Option<f64>,
    pub resolution_km: Option<f64>,
}

impl TileStatus {
    /// Convenience constructor for the common case (just a state, everything
    /// else null) — mirrors how the Python routers build a bare `TileStatus(status=...)`.
    pub fn new(status: TileState) -> Self {
        Self {
            status,
            key: None,
            png_url: None,
            bounds: None,
            band: None,
            cmap: None,
            product: None,
            satellite: None,
            sat_lon: None,
            scan_start: None,
            elapsed: None,
            message: None,
            center: None,
            width_km: None,
            resolution_km: None,
        }
    }
}
