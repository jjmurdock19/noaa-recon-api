//! noaa-recon-core — the WASM-safe compute core.
//!
//! Everything here must compile to **both** native (`x86_64-*`) and
//! **`wasm32-unknown-unknown`**. That means: no filesystem, no networking, no
//! threads-by-default, and no C-linked crates (no netcdf-C, no rusqlite, no
//! openssl). The native server (`crates/server`) wraps this with all of that;
//! the future `crates/wasm` exposes it to JavaScript via wasm-bindgen.
//!
//! As the port proceeds, the CPU-heavy, pure-math pieces of the Python app move
//! here: colormap lookup, Web-Mercator projection, resampling, day/night
//! terminator blending, and PNG/JPEG encoding — the parts a browser could run
//! itself against raw netCDF, no server round-trip.

pub mod bbox;
pub mod catalog;
pub mod colormap;
pub mod models;
pub mod project; // Web-Mercator + geostationary geometry (abi_to_latlon)
pub mod render; // array -> RGBA (gap-fill, smooth, colorize)
pub mod sweep; // TDR sweep slicing (CAPPI / vertical-profile plane extraction)

// The netCDF/HDF5 decode itself stays in crates/server (needs the C library);
// the render pipeline above takes decoded arrays as input, so it stays WASM-safe
// and the decode can later be swapped for a pure-Rust or browser-supplied
// backend.
