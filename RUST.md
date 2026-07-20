# noaa-recon-api — Rust port (`rust` branch)

This branch is a from-scratch Rust rewrite of the Python/FastAPI API on `main`,
built so the compute core can later compile to **WASM** for in-browser use. It's
a Cargo **workspace**:

```
crates/
  core/    → WASM-safe pure Rust (models; later: colormaps, projection, render)
  server/  → the native axum API server (this is what you deploy/benchmark)
```

The Rust and Python servers **share the same on-disk data** (`data/*.sqlite`,
`cache/`, `logs/`), so you can run either behind the same reverse proxy and
compare them directly.

---

## 1. Prerequisites (Windows, already set up on this machine)

- Rust (GNU toolchain): `rustup default stable-x86_64-pc-windows-gnu`
- WinLibs mingw-w64 on `PATH` (provides `gcc`/`dlltool`, needed to compile the
  bundled SQLite). Bin dir:
  `%LOCALAPPDATA%\Microsoft\WinGet\Packages\BrechtSanders.WinLibs.POSIX.MSVCRT_*\mingw64\bin`

- **CMake** + build-time `CFLAGS` for the `netcdf` static build (compiles
  HDF5 + netCDF-C from source). On this machine CMake is the portable zip at
  `%LOCALAPPDATA%\noaa-rust-tools\cmake-4.4.0-windows-x86_64\bin`. Newer GCC
  needs the permerror downgrade below or the netCDF-C build fails at ~4%.

On Linux (the eventual deploy target) you just need `rustc`/`cargo`, a C
compiler (`gcc`), and `cmake` — all standard; the CFLAGS workaround is only
needed with GCC ≥ 14.

## 2. Build & run

```sh
# from the repo root, on the `rust` branch. On Windows, ensure the mingw bin and
# the portable cmake bin are on PATH first, then:
export CC=gcc CXX=g++
export CFLAGS="-Wno-error=incompatible-pointer-types -Wno-error=int-conversion -Wno-error=implicit-function-declaration"
cargo build --release -p noaa-recon-api      # optimized binary (first build compiles HDF5+netCDF-C, ~minutes)
PORT=8000 ./target/release/noaa-recon-api    # serve on 127.0.0.1:8000
```

At runtime the binary needs the mingw runtime DLLs (`libgcc_s`, `libwinpthread`,
`libstdc++`) on PATH — add the WinLibs `mingw64\bin` dir.

Environment knobs (all optional):

| Var | Default | Meaning |
|-----|---------|---------|
| `PORT` | `8000` | listen port (same knob the Python side uses) |
| `NOAA_RECON_HOST` | `127.0.0.1` | bind address |
| `NOAA_RECON_REPO_ROOT` | cwd | where `data/`, `cache/`, `logs/`, `app/console/` live |
| `RUST_LOG` | `info` | log verbosity |

## 2b. Installing on a Linux server (systemd) via install.sh

`install.sh` now has a **version picker** — run it interactively and it asks
"Rust or Python", or pass it non-interactively:

```sh
# Rust variant (clones the `rust` branch, installs rust+cmake, cargo build --release,
# systemd runs the compiled binary):
sudo ./install.sh --variant rust
# Python variant (the original FastAPI app on main):
sudo ./install.sh --variant python
```

Important realities of the **Rust variant** install:
- It is **fully self-contained — no Python at all.** The server and every ingest/
  maintenance task are native subcommands (`ingest-storms`, `ingest-recon`,
  `clean-nc-cache`); no venv, no pip.
- First build compiles netCDF-C + HDF5 from source (a few minutes, needs cmake +
  a C compiler; both are installed for you).
- The systemd unit runs `target/release/noaa-recon-api` with `PORT` /
  `NOAA_RECON_HOST` / `NOAA_RECON_REPO_ROOT` in its environment.
- Domain *path-prefix* mode has no `--root-path` equivalent (subdomain/LAN/local
  are fine).

> To install from a fresh VM via `curl`, fetch **this branch's** installer
> (`raw.githubusercontent.com/<owner>/noaa-recon-api/rust/install.sh`) — the
> picker + Rust build path live here. Mirror `install.sh` to `main` if you want
> the canonical `main` installer to offer the choice too.

## 3. Populate data to test with

**All ingest is now native Rust subcommands — there is no Python on this branch:**

```sh
./target/release/noaa-recon-api ingest-storms                        # -> data/storms.sqlite (HURDAT2 + ATCF, ~10s)
./target/release/noaa-recon-api ingest-recon [--years Y,Y] [--full]  # -> data/recon_met.sqlite (crawl + netCDF + reconcile)
./target/release/noaa-recon-api clean-nc-cache [--max-age-hours N]   # prune cache/goes_nc
```

`ingest-recon` with no args does the current + previous season; `--full` does
every season since 2011 (several hours, hundreds of GB). With an empty DB the
endpoints still work — they just return empty/404. The installer's
`build_archives` step runs these automatically.

## 4. Quick smoke test

```sh
curl localhost:8000/v1/health                 # {"status":"ok"}
curl localhost:8000/v1/storms/years
curl localhost:8000/v1/storms/2023/LEE
curl localhost:8000/v1/recon/years
curl localhost:8000/v1/recon/mission/<id>
```

---

## 5. Port status (what works when you deploy this today)

| Area | Status |
|------|--------|
| `GET /v1/health` | ✅ ported |
| `/v1/storms/*` (years, list, track, nearest) | ✅ ported (read path) |
| `/v1/recon/*` (years, missions, mission detail, **source download stream**) | ✅ ported (read path) |
| `/v1/tdr/*` (years, missions, mission detail, sweep, volume, composite, plane_slice) | ✅ ported & verified (full pipeline: mission/file index, netCDF fetch/cache, 2D/3D slicing) |
| `/v1/raw/netcdf` | ⏳ 501 stub, same as Python |
| `/llms.txt`, `/cache`, `/demo/netcdf-three`, console static at `/` | ✅ ported |
| request logging + `stats` counter | ✅ ported |
| API-token store (SQLite, PBKDF2/SHA-256) | ✅ ported + unit-tested |
| Satellite **discovery** (`/satellite/products`, `/colortables`, `/colortable`, `/status`) | ✅ ported (colormaps/catalog live in WASM-safe `core`) |
| Satellite **imagery** (`/satellite/tile`) — single-band, all bands (2/3/5/7/9/13), bbox + full-disk | ✅ ported & verified end-to-end (real S3 fetch → netCDF decode → render → PNG) |
| Satellite imagery — composites (sandwich/geocolor) | ✅ ported & verified end-to-end (multi-band fetch → shared-canvas reproject → composite → PNG) |
| **API token gate** (`require_api_token`, off by default) | ✅ ported & verified (disabled=open, enabled=401 without token; health always open) |
| **Admin console backend** (`/v1/admin/*`) | ✅ ported & verified — login/logout/whoami (signed-cookie session), status, log tail, token CRUD, usage/login logs, auth-config, cache browse/delete, netCDF info |
| Admin console — self-update job | ✅ ported & verified (git fetch/pull/rebuild + restart) |
| Admin console — bulk prefetch, on-demand archive-update | ⏳ 501 (not wired up yet) |
| **Ingest — storms** (HURDAT2/ATCF), **recon MET** (crawl + netCDF + PDF + reconcile), **cache cleanup** | ✅ ported to Rust & verified live (`ingest-storms` / `ingest-recon` / `clean-nc-cache` subcommands) |

### Console login
On first run the server seeds a superuser from `admin_credentials.json`
(default `admin` / `password`) into `data/auth.sqlite` — change it via the
console's token management. Sessions are signed cookies keyed off that file's
`secret_key` (the analog of Starlette's `SessionMiddleware`).

### 100% Rust — no Python
This branch has **zero Python**: the API server *and* all data ingest/maintenance
are native Rust. `pyproject.toml`, `app/*.py`, and `scripts/*.py` are gone; only
`app/console/` (static UI assets, served by the binary) remains. The rust-variant
installer builds nothing Python — no venv, no pip.

### Remaining gaps
Admin console **bulk prefetch** and **on-demand archive-update** still return
`501` — the underlying ingest logic exists as CLI subcommands
(`ingest-storms`/`ingest-recon`), it's just not wired up as background jobs
behind the console's HTTP endpoints yet. Everything else — IR + reflectance
imagery, both composite products, all data endpoints, all ingest, auth,
self-update, and the rest of the console — works.

### Solved: reflectance bands + composites needed the zstd HDF5 filter
GOES **IR** bands (7/9/13) compress their `CMI` with zlib/deflate — included in
the static HDF5 build, so they've always decoded and rendered correctly. The
**reflectance** bands (2/3/5) and the composite products that use them
(`sandwich`/`geocolor`) compress with **zstd** (HDF5 filter 32015), which
netCDF-C ships only as a *dynamically-loaded* HDF5 plugin
(`plugins/H5Zzstd.c`) — its own CMake build ties `NETCDF_ENABLE_PLUGINS` to
`BUILD_SHARED_LIBS`, so it's unavailable when statically linking, static build
or not, on any OS.

Rather than switching to a dynamically-linked system netCDF (extra runtime
dependency, breaks the single-binary deploy story) or forking `netcdf-src`'s
CMake to statically compile the plugin in (drifts from upstream), the fix
registers the zstd filter with HDF5 **programmatically** at process startup:
`crates/server/src/services/hdf5_zstd.rs` hand-implements a decode-only HDF5
filter (`H5Z_class2_t`) using `zstd-sys` — a normal, offline, statically-linked
Cargo dependency, no different from the netCDF/HDF5 crates already in the
tree — and calls `H5Zregister()` before any netCDF file is opened
(`main.rs`). HDF5's chunk cache then finds filter 32015 in its internal table
the same way it already finds zlib; no dynamic plugin loading, no system
package, no fork to maintain.

Composite products (`sandwich`/`geocolor`) are implemented in
`crates/core/src/render.rs` (`render_sandwich`/`render_geocolor` — pure array
math, WASM-safe) with the multi-band S3 fetch/decode orchestration in
`crates/server/src/services/goes.rs` (`render_product_and_store`).

### GOES architecture (in progress)
The colormaps, band/cmap catalog, and bbox math are ported into `crates/core`
(WASM-safe, unit-tested for parity). The plan for `/tile`: the whole render
pipeline (projection, gap-fill, smoothing, colorize, composites) is pure array
math and also goes into `core`; only the **netCDF decode + S3 fetch** stay in
`crates/server`. That keeps the renderer WASM-compilable and isolates the one
C-library dependency behind the decode step.
