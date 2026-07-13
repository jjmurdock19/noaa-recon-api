//! HTTP routers — one module per `app/routers/*.py`.
//!
//! Each submodule exposes a `pub fn router() -> Router<AppState>` that
//! `main.rs` nests under `/v1`, mirroring FastAPI's `include_router(..., prefix="/v1")`.
//! Ported incrementally; only `health` exists so far.

pub mod admin;
pub mod admin_tokens;
pub mod health;
pub mod raw;
pub mod recon;
pub mod satellite;
pub mod storms;
pub mod tdr;
