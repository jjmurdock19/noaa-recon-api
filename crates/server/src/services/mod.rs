//! Service layer — one module per `app/services/*.py`.
//!
//! These are native-only (SQLite, HTTP, filesystem); the WASM-safe compute
//! lives in `noaa-recon-core`. Ported incrementally.

pub mod archive_update;
pub mod cache;
pub mod downloads;
pub mod goes;
pub mod hdf5_zstd;
pub mod recon_ingest;
pub mod recon_met;
pub mod self_update;
pub mod stats;
pub mod storms;
pub mod tokens;
