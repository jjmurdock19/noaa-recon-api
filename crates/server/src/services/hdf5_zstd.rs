//! Registers the zstd HDF5 filter (id 32015) with the HDF5 library at
//! process startup, so netCDF's chunk-decode path can decompress GOES
//! reflectance-band (2/3/5) `CMI` data.
//!
//! Background: GOES compresses those bands with zstd (HDF5 filter 32015).
//! netCDF-C ships zstd support only as a *dynamically-loaded* HDF5 plugin
//! (`plugins/H5Zzstd.c`, discovered via `dlopen` against a plugin search
//! path) — its own CMake build ties `NETCDF_ENABLE_PLUGINS` to
//! `BUILD_SHARED_LIBS`, so it's unavailable when statically linking (see
//! RUST.md). IR bands (7/9/13) use plain zlib/deflate, which the static
//! HDF5 build includes directly, so they've always worked.
//!
//! HDF5 doesn't actually require the dynamic-plugin path, though — a filter
//! can be registered programmatically via `H5Zregister()` with an
//! `H5Z_class2_t` describing its callbacks, entirely independent of the
//! `H5PL` plugin-discovery mechanism. That's what this module does: a
//! hand-written decode-only zstd filter (we only ever read GOES archives,
//! never write compressed netCDF) built from the `zstd-sys` crate (a normal
//! statically-linked Cargo dependency — no system package, no forked CMake,
//! no dynamic loading at runtime). Once registered, HDF5's chunk cache finds
//! filter 32015 in its internal table the same way it already finds zlib.
//!
//! The `H5Z_class2_t` struct layout and `H5Zregister`/`H5allocate_memory`/
//! `H5free_memory` signatures are part of HDF5's stable public C ABI
//! (unchanged since HDF5 1.8.3); the field types here mirror
//! `hdf5-metno-src`'s vendored `H5Zdevelop.h`/`H5public.h` exactly. We link
//! them via `extern "C"` rather than generating bindings because the symbols
//! are already pulled into the final binary by the `netcdf`/`hdf5-metno-sys`
//! crates this module doesn't otherwise depend on.

use std::ffi::{c_char, c_int, c_uint, c_void};
use std::sync::Once;

const H5Z_FILTER_ZSTD: c_int = 32015;
const H5Z_CLASS_T_VERS: c_int = 1;
const H5Z_FLAG_REVERSE: c_uint = 0x0100;

type HTri = c_int; // htri_t
type HId = i64; // hid_t
type HErr = c_int; // herr_t

type CanApplyFn = extern "C" fn(dcpl_id: HId, type_id: HId, space_id: HId) -> HTri;
type SetLocalFn = extern "C" fn(dcpl_id: HId, type_id: HId, space_id: HId) -> HErr;
type FilterFn = extern "C" fn(
    flags: c_uint,
    cd_nelmts: usize,
    cd_values: *const c_uint,
    nbytes: usize,
    buf_size: *mut usize,
    buf: *mut *mut c_void,
) -> usize;

/// Mirrors HDF5's `H5Z_class2_t` (see `H5Zdevelop.h`) field-for-field.
#[repr(C)]
struct H5ZClass2 {
    version: c_int,
    id: c_int, // H5Z_filter_t
    encoder_present: c_uint,
    decoder_present: c_uint,
    name: *const c_char,
    can_apply: Option<CanApplyFn>,
    set_local: Option<SetLocalFn>,
    filter: Option<FilterFn>,
}

// H5Z_class2_t only ever gets read by HDF5, never mutated; safe to share
// across the single `H5Zregister` call at startup.
unsafe impl Sync for H5ZClass2 {}

extern "C" {
    fn H5Zregister(cls: *const c_void) -> HErr;
    fn H5allocate_memory(size: usize, clear: c_uint) -> *mut c_void;
    fn H5free_memory(mem: *mut c_void) -> HErr;
}

extern "C" fn can_apply(_dcpl_id: HId, _type_id: HId, _space_id: HId) -> HTri {
    1 // always applicable, matching netCDF's own H5Zzstd.c
}

/// Decode-only: this server never writes zstd-compressed netCDF, so the
/// `H5Z_FLAG_REVERSE` (decompress) branch is the only one implemented. A
/// compress attempt fails loudly (returns 0) rather than silently
/// mis-encoding.
extern "C" fn filter_zstd(
    flags: c_uint,
    _cd_nelmts: usize,
    _cd_values: *const c_uint,
    nbytes: usize,
    buf_size: *mut usize,
    buf: *mut *mut c_void,
) -> usize {
    if flags & H5Z_FLAG_REVERSE == 0 {
        tracing::error!("zstd HDF5 filter: compression not supported (decode-only build)");
        return 0;
    }

    unsafe {
        let inbuf = *buf as *const u8;
        let inbuf_len = nbytes;

        let content_size = zstd_sys::ZSTD_getFrameContentSize(inbuf as *const c_void, inbuf_len);
        if content_size == zstd_sys::ZSTD_CONTENTSIZE_ERROR as u64
            || content_size == zstd_sys::ZSTD_CONTENTSIZE_UNKNOWN as u64
        {
            tracing::error!("zstd HDF5 filter: could not determine decompressed frame size");
            return 0;
        }
        let outbuf_len = content_size as usize;

        let outbuf = H5allocate_memory(outbuf_len, 0);
        if outbuf.is_null() {
            tracing::error!("zstd HDF5 filter: allocation failed ({outbuf_len} bytes)");
            return 0;
        }

        let written =
            zstd_sys::ZSTD_decompress(outbuf, outbuf_len, inbuf as *const c_void, inbuf_len);
        if zstd_sys::ZSTD_isError(written) != 0 {
            let msg = std::ffi::CStr::from_ptr(zstd_sys::ZSTD_getErrorName(written));
            tracing::error!("zstd HDF5 filter: decompress failed: {}", msg.to_string_lossy());
            H5free_memory(outbuf);
            return 0;
        }

        H5free_memory(*buf);
        *buf = outbuf;
        *buf_size = outbuf_len;
        written
    }
}

static CLASS: H5ZClass2 = H5ZClass2 {
    version: H5Z_CLASS_T_VERS,
    id: H5Z_FILTER_ZSTD,
    encoder_present: 0,
    decoder_present: 1,
    name: b"zstd\0".as_ptr().cast(),
    can_apply: Some(can_apply),
    set_local: None,
    filter: Some(filter_zstd),
};

static REGISTER_ONCE: Once = Once::new();

/// Idempotent; safe to call more than once (only the first call registers).
/// Must run before the first netCDF file with zstd-compressed chunks is
/// opened — called once at server startup (see `main.rs`).
pub fn register() {
    REGISTER_ONCE.call_once(|| {
        let rc = unsafe { H5Zregister(&CLASS as *const H5ZClass2 as *const c_void) };
        if rc < 0 {
            tracing::error!("H5Zregister(zstd) failed (rc={rc}) — reflectance-band tiles (2/3/5) will read as fill/NaN");
        } else {
            tracing::info!("Registered zstd HDF5 filter (id {H5Z_FILTER_ZSTD}) — reflectance bands decode natively");
        }
    });
}
