//! Service layer — one module per `app/services/*.py`.
//!
//! These are native-only (SQLite, HTTP, filesystem); the WASM-safe compute
//! lives in `noaa-recon-core`. Ported incrementally.

pub mod cache;
pub mod goes;
pub mod recon_ingest;
pub mod recon_met;
pub mod stats;
pub mod storms;
pub mod tokens;
