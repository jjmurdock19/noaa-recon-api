# noaa-recon-api

**Open-source HTTP API for archival GOES satellite imagery and NOAA Tail
Doppler Radar data.** CORS-open, no auth, no API key — built for a
hurricane tracker site, designed to be called from any website.

[![License: MIT](https://img.shields.io/badge/license-MIT-green.svg)](LICENSE)
[![Live API](https://img.shields.io/badge/API-live-brightgreen)](https://joshmurdock.net/api/docs)

📖 **[API.md](API.md)** — full endpoint reference, every param, copy-paste
curl/JavaScript/Python examples
🤖 **[llms.txt](llms.txt)** — terse agent-discovery summary, also served
live at `{base}/llms.txt`

## Deploy your own copy

Want to run this on your own machine instead of calling the hosted one?
One command gets you a fully configured, self-updating instance —
dependencies, systemd service, nginx/Apache + HTTPS if you want a domain,
and the storm/recon archives, all via an interactive wizard:

```bash
bash -c "$(curl -fsSL https://raw.githubusercontent.com/jjmurdock19/noaa-recon-api/main/install.sh)"
```

Works on Fedora/RHEL/Rocky/CentOS (`dnf`), Debian/Ubuntu (`apt`), and
anything running the Nix package manager. See **[INSTALL.md](INSTALL.md)**
for a plain-language walkthrough of every question it asks, or the
["Manual setup"](#manual-setup) section below to do each step by hand.

---

## Hurricane Melissa, rendered by this API

Real output from `GET /v1/satellite/tile` — GOES-19, Band 13 (Clean
Longwave IR), the `abi13` standard enhancement, a 1000 nautical-mile box
centered on the storm (17.55°N, 78.14°W, 2025-10-28 12:00 UTC):

![Hurricane Melissa, GOES-19 Band 13, abi13 enhancement](docs/assets/melissa-abi13.jpg)

```bash
curl "https://joshmurdock.net/api/v1/satellite/tile?time=2025-10-28T12:00:00Z&band=13&center=17.55,-78.14&dims=1000&unit=nm"
```

## What it does

- **Archival GOES satellite tiles on demand.** Give it any UTC timestamp
  (not just hourly buckets) and it finds the nearest real ABI scan
  (~10-minute cadence), downloads it from NOAA's public S3 archive,
  reprojects it, and returns a georeferenced PNG ready to drop onto a
  Leaflet map.
- **Both GOES-East and GOES-West**, auto-resolved to the correct satellite
  (GOES-16/19 East, GOES-17/18 West) for the requested date — covers the
  full ABI era (~2017-2018 onward). Pre-ABI storms (e.g. Katrina, 2005)
  aren't reachable this way; see "Satellite coverage" in API.md for why.
- **The correct color table for the band you asked for.** `cmap=default`
  resolves to the right per-band standard enhancement — `abi13` (Clean
  IR), `abi9` (water vapor), `abi7` (shortwave IR / "fire temperature"),
  or `abi5` (near-IR reflectance) — built from exact temperature→color
  stops (or a reflectance ramp for band 5), not a generic approximation.
  See [the live color legend tool](#color-legend) below.
- **Composite products**: `product=sandwich` (Band 13 IR modulated by
  Band 2 visible texture) and `product=geocolor` (a documented
  approximation of NOAA's day/night true-color+IR composite — see `GET
  /v1/satellite/products` for exactly what's simplified). Full-disk only
  for now.
- **`GET /v1/satellite/products`** — discovery endpoint listing every band/
  product this API can render and the exact UTC date range each satellite
  covers, so a client can build a picker without hardcoding any of that.
- **Fast, high-detail regional crops.** Pass `center` + `dims` (km or
  nautical miles) instead of rendering the slow, coarse full disk —
  ~11x faster, ~130x smaller files, at the sensor's native ~2km/pixel
  resolution by default.
- **Correctly georeferenced for web maps.** Output rows are spaced in Web
  Mercator, matching how Leaflet/Google Maps/every standard web map
  actually projects the earth — see "Real bugs found and fixed" below.
- **A raw-netCDF path for client-side rendering** (in progress) feeding a
  Three.js/WebGL volumetric viewer, for users who want the data itself
  rather than a pre-rendered image.
- **Historical storm tracks, named storms only.** Feed a year, storm
  name, and any UTC datetime and get back the closest actual best-track
  fix — lat/lon, Saffir-Simpson category, wind, and pressure. Backed by
  NHC's HURDAT2 archive (Atlantic + East/Central Pacific since 1950) for
  the reconciled historical record, plus NHC's operational ATCF b-decks to
  bridge the gap up through the storm happening right now — kept current
  by a nightly systemd timer. Plus discovery endpoints for what
  years/storms are on record. See `GET /v1/storms/*` in API.md and "Storm
  archive updates" below.
- **Recon MET archive — every hurricane hunter flight since 2011.**
  Look up archived flight-level observation data (position, wind,
  SFMR surface wind, altitude) by year and storm name, then fetch one
  mission by `mission_id` — decimated data inline for quick plotting, or
  `GET /v1/recon/mission/{id}/download` to stream NOAA's original
  full-resolution NetCDF file (600+ variables — attitude, airspeed, every
  raw sensor channel, not just the ~7 fields this project decimates).
  Same year/storm discovery shape as the storm-track archive, so the two
  can be cross-referenced from one API. Kept current by its own nightly
  systemd timer. See `GET /v1/recon/*` in API.md and "Recon MET archive" below.

**Status:** MVP. Satellite tiles, storm tracks, and the recon MET archive
are fully implemented and verified against live NOAA data. Band
2/GeoColor, TDR, and the raw-netCDF passthrough are stubbed (`501 Not
Implemented`) — see "Roadmap" below.

## Color legend

`GET /v1/satellite/colortable` returns the exact stops a render actually
used, so a client can show a legend that's guaranteed to match — this is
what powers the live gradient legend in the hurricanes site's API
explorer panel:

```bash
curl "https://joshmurdock.net/api/v1/satellite/colortable?cmap=default&band=13"
```

## Try it right now

No setup needed — it's already deployed:

```bash
curl "https://joshmurdock.net/api/v1/satellite/tile?time=$(date -u +%FT%TZ)&band=13"
```

See [API.md](API.md) for the full reference and curl/JavaScript/Python
integration examples.

---

## Architecture

```mermaid
flowchart LR
    subgraph Clients
        A[Hurricane tracker site<br/>js/api-explorer.js]
        B[Any other website]
        C[netcdf-three demo<br/>browser WebGL]
    end

    subgraph "noaa-recon-api (FastAPI)"
        D["/v1/satellite/tile<br/>/v1/satellite/status<br/>/v1/satellite/colortable"]
        E["/v1/tdr/* (planned)"]
        F["/v1/raw/netcdf (planned)"]
        G[ResultCache<br/>lock-file + TTL]
        H[app/services/goes.py<br/>ABI reprojection + colortables]
    end

    I[(NOAA S3<br/>noaa-goes16/17/18/19)]
    J[(NOAA TDR archive<br/>seb.omao.noaa.gov, planned)]

    A & B -->|HTTP, CORS open| D
    C -.->|planned| F
    D --> G
    G --> H
    H -->|public bucket, no auth| I
    E -.-> J
```

## Request flow (satellite tile)

```mermaid
sequenceDiagram
    participant Client
    participant API as noaa-recon-api
    participant S3 as NOAA S3

    Client->>API: GET /v1/satellite/tile?time=...&band=13
    API->>API: resolve_nearest() — find closest scan to `time`
    alt cache hit
        API-->>Client: {status: "ready", png_url, bounds, ...}
    else cache miss
        API-->>Client: {status: "generating", key}
        API->>S3: download ABI-L2-CMIPF netCDF (~25MB)
        API->>API: reproject (Web Mercator) + colorize + render PNG
        loop poll every ~3s
            Client->>API: GET /v1/satellite/status/{key}
            API-->>Client: {status: "generating", elapsed} or {status: "ready", ...}
        end
    end
    Client->>API: GET {png_url}
    API-->>Client: image/png (georeferenced tile)
```

---

## Manual setup

*Deploying to a server and want the systemd service, nginx/HTTPS, and
archives set up for you instead? Use `./install.sh` — see
[INSTALL.md](INSTALL.md) or "Deploy your own copy" above. What follows
is the same thing done by hand, step by step, for local dev or anyone
who wants full manual control.*

```bash
git clone --recurse-submodules <this-repo-url>
cd noaa-recon-api
python3 -m venv .venv
source .venv/bin/activate
pip install -e ".[dev]"

uvicorn app.main:app --reload
# -> http://127.0.0.1:8000/docs   (Swagger UI, full endpoint surface)
# -> http://127.0.0.1:8000/       (admin console — see below)
```

Already cloned without `--recurse-submodules`? Run `git submodule update --init`.

Try it:

```bash
curl "http://127.0.0.1:8000/v1/satellite/tile?time=2024-09-28T12:00:00Z&band=13"
# -> {"status": "generating", "key": "goes_13_abi13_..."}
curl "http://127.0.0.1:8000/v1/satellite/status/<key>"
# -> poll until {"status": "ready", "png_url": "/cache/satellite/<key>.png", "bounds": [[lat,lon],[lat,lon]], ...}
```

### Tests

```bash
pytest                                          # offline unit tests (math, LUTs, parsing)
NOAA_RECON_API_NETWORK_TESTS=1 pytest           # + a live end-to-end render against NOAA S3
```

### Docker

```bash
docker compose up --build
```

### Deploying on this host (joshmurdock.net)

*Already done — this is documented for reference / redeploying after a host
rebuild. The live API is at `https://joshmurdock.net/api`.*

1. `python3 -m venv .venv && pip install -e .` as above.
2. Copy `deploy/noaa-recon-api.service` to `/etc/systemd/system/`, then
   `systemctl daemon-reload && systemctl enable --now noaa-recon-api.service`.
3. Paste the block from `deploy/nginx-snippet.conf` into the `joshmurdock.net`
   `server {}` block in `/etc/nginx/nginx.conf`, then `nginx -t && systemctl
   reload nginx`. This makes the API reachable at `/api/...` on the
   hurricanes site, same-origin (no CORS needed for that consumer; CORS is
   still open for other sites hitting the API directly).

### Deploying elsewhere (fresh host, building both archives from scratch)

*`./install.sh` does everything below automatically, including asking
whether to build the full recon archive or the fast current-season-only
version. This section is the manual equivalent, for anyone not using it.*

Both `GET /v1/storms/*` and `GET /v1/recon/*` are backed by local SQLite
databases under `data/` (gitignored — not part of the repo, built by the
ingestion scripts below). A brand-new deployment has neither database yet;
build both once, then install the nightly timers so they stay current
without further attention:

```bash
# Storms: always does a full HURDAT2 + ATCF backfill automatically (fast, ~10s)
.venv/bin/python3 scripts/ingest_storms.py

# Recon MET: --full crawls every year since 2011 from scratch — this is
# thousands of small requests plus large NetCDF downloads and PDF parses,
# expect this to take HOURS on a fresh deployment (the nightly default,
# current + previous year only, is what runs afterward and is quick)
.venv/bin/python3 scripts/ingest_recon_met.py --full
```

Then install both nightly timers (details in the two sections below) so
each archive keeps itself current going forward without manual re-runs.
The admin console's "Databases" panel also has a **Force update** button
per archive if you ever want to trigger an update immediately instead of
waiting for the timer (e.g. right after a storm you care about was flown).

### Storm archive updates

`data/storms.sqlite` (the `GET /v1/storms/*` backing store) is populated
by `scripts/ingest_storms.py` — see app/services/storms.py's module
docstring for the HURDAT2 + ATCF b-deck pipeline. Run it manually any time:

```bash
.venv/bin/python3 scripts/ingest_storms.py
```

To keep it current automatically (picks up the latest advisory for any
storm active right now), install the nightly timer:

```bash
sudo cp deploy/storm-archive-update.service deploy/storm-archive-update.timer /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now storm-archive-update.timer
```

Fires nightly at 03:15 (server local time); `Persistent=true` means a
missed run (host was down) fires on next boot instead of waiting a full
day. Both units show up in Cockpit's Services page like any other systemd
unit — search "storm-archive-update" to check last-run status or trigger
a manual run from there instead of the command line.

### Recon MET archive

`data/recon_met.sqlite` (the `GET /v1/recon/*` backing store) is populated
by `scripts/ingest_recon_met.py` — see app/services/recon_met.py's module
docstring for the crawl/decimation pipeline (NOAA's raw 1-second
flight-level data at `seb.omao.noaa.gov`, stored at 0.2 Hz). Run it
manually any time:

```bash
.venv/bin/python3 scripts/ingest_recon_met.py               # current + previous year (nightly default)
.venv/bin/python3 scripts/ingest_recon_met.py --full         # every year since 2011 — see the warning above
.venv/bin/python3 scripts/ingest_recon_met.py --year 2024    # one year only
```

Idempotent: each mission is skipped unless its QC version on the server
changed, so a nightly re-run only does real work for new/upgraded
missions. Install the nightly timer the same way:

```bash
sudo cp deploy/recon-met-update.service deploy/recon-met-update.timer /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now recon-met-update.timer
```

Fires nightly at 03:45 (30 minutes after the storm archive timer, so the
two crawls don't compete for network/CPU at the same instant);
`Persistent=true` for the same missed-run-catches-up-on-boot behavior.
Also visible in Cockpit's Services page (search "recon-met-update").

This deployment's `data/recon_met.sqlite` was seeded via
`scripts/import_existing_met_archive.py`, a one-time local copy from the
hurricanes site's already-harvested `met_archive.sqlite` (same underlying
data, this project just now owns the feature) rather than re-crawling
years of identical data over the network — not needed on a fresh
deployment with no pre-existing archive, hence `--full` above instead.

### Admin console

Visiting the API's root (`/` locally, `https://joshmurdock.net/api` in
production) serves a login-gated admin console — status/cache stats,
browsing and deleting cached rendered tiles and raw netCDF downloads,
submitting a one-off query, and bulk-loading a timeframe into the cache
(e.g. pre-warm an entire storm's lifecycle in one request instead of
loading frame-by-frame later). A "Databases" panel covers the storm-track
and recon MET archives: size/record-count cards folded into the same
overall totals, a browsable viewer (pick a database, year, and storm to
see its track points or missions), and a **force update** button per
archive to run that archive's nightly ingest immediately instead of
waiting for the timer.

Default credentials are `admin` / `password`, stored in
**`admin_credentials.json`** at the repo root (created automatically with
those defaults on first run if it doesn't exist — gitignored, never
committed). **Change the password before exposing this publicly** — edit
that file directly, it's plain JSON:

```json
{
  "username": "admin",
  "password": "your-new-password",
  "secret_key": "<auto-generated, leave alone — used to sign session cookies>"
}
```

Auth is a signed-cookie session (Starlette's `SessionMiddleware` +
`itsdangerous`) — proportionate to a single-operator admin tool sitting
behind nginx/HTTPS, not a full multi-user auth system. The console itself
is a static page (`app/console/index.html`, no build step, matching the
rest of this project) that calls the JSON endpoints under
`/v1/admin/*` (see API.md).

### netcdf-three demo client

`clients/netcdf-three-demo/index.html` is a static page (no build step) that
proves the raw-netCDF → browser-rendering path end-to-end using the
[netcdf-three](https://github.com/umrlastig/netcdf-three) library, vendored
as a git submodule at `clients/netcdf-three-demo/vendor/netcdf-three`.

```bash
cd clients/netcdf-three-demo
python3 -m http.server 8765
# -> open http://127.0.0.1:8765/ in a browser
```

It defaults to the sample dataset bundled with the netcdf-three submodule.
**This has been verified to serve correctly (all assets return 200, the
sample file parses and contains real 3D variables) but has not been visually
verified in an actual browser** — there's no display/headless-browser
available in the environment this was built in. Open it in a real browser
and confirm the volume actually renders before relying on it.

---

## Agentic instructions

This section is for an AI agent picking up one of the roadmap items below
without re-deriving the architecture.

### Repo shape

```
app/
  main.py            FastAPI app, CORS (open — this API is meant for other sites too), configure_logging()
                      (called at import time, before anything else logs), the log_requests middleware
                      (one line per request to logs/app.log), SessionMiddleware (admin console auth),
                      router includes, /cache + /demo/netcdf-three static mounts, GET /llms.txt, and the
                      console static mount at "/" (registered LAST so it doesn't shadow the more specific
                      routes above it).
  auth.py            Admin console auth: loads/creates admin_credentials.json (gitignored, repo root —
                      see README "Admin console"), verify_credentials(), require_login() FastAPI dependency.
  logging_config.py    configure_logging(): rotating file handler (10MB x5) attached to the ROOT logger
                        ONLY — attaching to "uvicorn.error"/"uvicorn.access" too caused every line to be
                        logged twice (they propagate to root by default; found and fixed empirically, see
                        the module docstring). app.* loggers (e.g. goes.py's `log`) need no extra setup.
  paths.py            CACHE_ROOT = <repo>/cache, REPO_ROOT
  models.py            Pydantic response schemas
  console/index.html   Static admin console UI (no build step) — login form + dashboard + cache preview
                        pane (tile images w/ full metadata, netCDF dimension/variable/attribute inspection),
                        calls /v1/admin/*. All requests are prefixed with a runtime-computed API_BASE (see
                        "Real bugs" below) — never hardcode a path starting with "/" in this file.
  routers/
    satellite.py       GET /v1/satellite/tile, /status/{key}, /colortable  — IMPLEMENTED
    admin.py            GET/POST /v1/admin/* — login/logout/whoami, status, cache list/delete (satellite
                         + goes_nc separately, goes_nc also has a GET .../info for netCDF structural
                         metadata), bulk prefetch (POST /prefetch, GET /prefetch/{job_id}). All except
                         login/whoami require require_login(). Prefetch jobs are tracked in an in-memory
                         dict (fine for this scope — lost on restart, not persisted).
    tdr.py              GET /v1/tdr/missions, GET /v1/tdr/sweep                — STUB (501)
    raw.py              GET /v1/raw/netcdf                                     — STUB (501)
    health.py           GET /v1/health
  services/
    goes.py             Ported from the hurricanes site's goes_tile.py: ABI Fixed Grid reprojection
                         (PUG Vol 5 Sec 4.2), Web Mercator row spacing (_mercator_y — see "Real bugs"
                         below), collision-safe forward-paint (_paint_coldest), per-band "default ABI"
                         colortables (abi13, abi9 — exact stops, evaluated without LUT quantization via
                         _apply_stops_exact/STOPS_BY_CMAP) plus 5 LUT-based approximate tables in LUTS.
                         resolve_nearest() picks the closest scan to an arbitrary timestamp for true
                         ~10-min resolution. render_bbox_to_png() is a two-pass sparse-locate +
                         native-resolution-crop renderer for center+dims requests (render_to_png() is
                         the full-disk path).
    cache.py            ResultCache: lock-file + TTL pattern (mirrors proxy.php's approach),
                         driven by FastAPI BackgroundTasks instead of subprocess/nohup. Also has
                         list_keys()/delete()/stats() for the admin console's cache browser.
    tdr.py              Empty stub — see its docstring for the planned crawler/parse/render shape.
admin_credentials.json        Gitignored, auto-created on first run — admin console login (see README).
logs/app.log                  Gitignored, auto-created on first run — rotating request/error log (see API.md "Logging").
clients/netcdf-three-demo/   Static demo client (see "netcdf-three demo client" above)
deploy/                       nginx snippet + systemd unit for this specific host (joshmurdock.net) —
                               install.sh generates its own versions of these for other machines
install.sh                    Interactive installer/updater/uninstaller (dnf/apt/nix) — see INSTALL.md
scripts/render_ascii_logo.py  Regenerates install.sh's terminal banner from assets/branding/noaa_logo.bbcode
docs/assets/                  README example images
docs/colortable_sources/      Source JSON for the abi13/abi9 exact color stops
tests/test_satellite.py       Offline math/parsing tests + one network-gated e2e test
API.md                        Full human+agent endpoint reference, kept in sync with routers/ by hand —
                               if you add/change an endpoint, update this file and llms.txt in the same change.
llms.txt                      Terse agent-discovery summary (llmstxt.org convention); also served live at
                               GET /llms.txt (app/main.py) — keep both in sync with reality, not aspiration.
```

### Real bugs already found and fixed here

1. **180° longitude flip.** The original `goes_tile.py` (and by extension
   this port, before the fix) had `Sx` defined with the wrong sign in
   `abi_to_latlon()`, which silently rotates every computed longitude by
   180° — `lat` is unaffected (it only depends on `Sx**2`) so it's easy to
   miss, but it makes the renderer paint ~0% of pixels onto the output grid
   and produce a blank tile. Fixed by computing
   `Sx = H - rs*cos(x)*cos(y)` per PUG Vol 5 Sec 4.2 (not
   `rs*cos(x)*cos(y) - H`). **The same bug likely exists in the live
   hurricanes site's `goes_tile.py`** — flag it to a person before touching
   that file, since it's in active production use.
   `tests/test_satellite.py::test_abi_to_latlon_subsatellite_point_is_origin`
   guards against a regression.

2. **Color-table quantization smearing.** `abi13`/`abi9` were originally
   built through the same shared 256-bucket LUT system (`_build_lut`/
   `_t2i`/`_i2t`) as the other colortables, which quantizes the full
   temperature range into ~0.6°C steps. That's fine for smooth gradients,
   but `abi13`'s source data has a deliberate 1°C-wide hard cut
   (cyan@-32°C → light grey@-31°C) which quantization smeared into a muddy
   blended color absent from the source palette, and the LUT's fixed
   -113..+42°C window clamped `abi13`'s warm end (needs up to +57°C) before
   it ever reached true black. Fixed by evaluating `abi13`/`abi9` exactly,
   per-pixel, via `_apply_stops_exact()` (vectorized `np.interp`) instead of
   routing them through the shared LUT — see the comment above `LUTS` in
   `app/services/goes.py`.
   `tests/test_satellite.py::test_apply_stops_exact_matches_direct_function_with_no_quantization`
   guards against a regression. **If you add another colortable with a
   sharp transition or a range outside ~-113..+42°C, add it to
   `STOPS_BY_CMAP` instead of `LUTS`, not the other way around.**

3. **Forward-paint collision loss.** The reprojection paint step used plain
   `output[row, col] = values` assignment. When multiple source pixels land
   on the same output cell (real and not rare — ~330 of ~51k cells on a
   typical 500km/native-resolution bbox render), numpy keeps an arbitrary
   one (whichever is last in array order), not the meteorologically
   significant one. Verified cell-by-cell on a real render: 160 cells
   differed between old and fixed behavior, with up to a 23°C difference at
   one cell — enough to jump entire color zones. Fixed via
   `_paint_coldest()` using `np.minimum.at` to deterministically keep the
   coldest value on collision.
   `tests/test_satellite.py::test_paint_coldest_keeps_minimum_on_collision`
   covers it.

4. **Equirectangular vs. Web Mercator mismatch (georeferencing).**
   `L.imageOverlay` (and every standard web map) positions an image's
   corners at the map's *Web Mercator* screen coordinates for the given
   bounds, then stretches the raw image **linearly** between them. The
   renderer was spacing output rows linearly by *latitude*
   (`row = f(lat)`, plain equirectangular/Plate Carrée), which doesn't
   match — the displayed imagery was vertically mispositioned, worse away
   from the image's vertical center and worse at higher latitudes. Fixed by
   spacing rows linearly in Web Mercator Y instead (`_mercator_y()`); the
   column/longitude mapping was already correct, since Web Mercator
   *is* linear in longitude. Verified on a real Hurricane Melissa render
   (1000nm box, 17.55°N–25.9°N): the storm's true latitude landed ~11px
   off-position (out of 926, ~1.2%) under the old method. The effect grows
   with box size and latitude, so it's far more severe for the full-disk
   render path (spans ±81.3°) or any higher-latitude storm.
   `tests/test_satellite.py::test_mercator_row_spacing_differs_from_linear_latitude`
   guards against a regression.

   ![Before (plain latitude spacing) vs after (Web Mercator spacing) — same scan, same bbox](docs/assets/mercator-fix-before-after.jpg)

5. **Admin console fetch requests used domain-root absolute paths.** The
   console (`app/console/index.html`) used literal absolute paths like
   `fetch('/v1/admin/login', ...)`. In production the page is served at
   `/api/` (nginx proxies `/api/` → this app's `/`), so the browser
   resolved `/v1/admin/login` against the domain root
   (`joshmurdock.net/v1/admin/login`, a 404 never proxied to us) rather than
   the page's own directory (`joshmurdock.net/api/v1/admin/login`). FastAPI's
   own request-handling was never even reached — but my generic `!res.ok`
   handler showed "Invalid username or password" for any non-OK response,
   masking the real 404 behind a plausible auth error. Fixed by computing
   `API_BASE = window.location.pathname.replace(/\/$/, '')` at page load and
   prefixing every fetch. **If you add new fetch() calls to the console, you
   MUST prefix them with `API_BASE + '/v1/...'` — never a bare `/v1/...`
   string.** Only caught because a user attempted login in production (where
   the `/api/` prefix exists); local dev testing (`127.0.0.1:8000`, no
   prefix) didn't trigger it. No automated test covers this — a real browser
   click-test is the only reliable check.

6. **Swagger UI 404 on `/openapi.json`.** FastAPI's auto-generated `/docs`
   page hardcodes the `url` it fetches the OpenAPI schema from as a simple
   `openapi.json` reference, which FastAPI rewrites to an absolute URL based
   on `root_path`. Without `root_path=/api`, the URL became the bare
   `/openapi.json` (domain root, not proxied to our app) instead of
   `/api/openapi.json`. Fixed by adding `--root-path /api` to the production
   uvicorn command in `deploy/noaa-recon-api.service` — local dev (`uvicorn
   app.main:app --reload`, no flag) is unaffected since there's no proxy
   prefix there. Same root-cause class as bug #5 (absolute paths constructed
   without awareness of a reverse-proxy path prefix), same symptom (works
   locally, breaks in production), same diagnostic approach (check what URL
   the failing fetch is actually constructing by inspecting the rendered HTML
   or browser devtools).

7. **netCDF4/HDF5 concurrent-access crash + reflectance-band OOM.** Found
   while adding the sandwich/geocolor composites: `BackgroundTasks` runs
   synchronous task functions in a thread pool, so two renders landing at
   the same moment call into `netCDF4`/HDF5 from different threads
   simultaneously — HDF5 isn't guaranteed thread-safe for this, and it
   reproducibly crashed the whole process (`double free or corruption`)
   the first time two composite renders (each opening several files)
   overlapped in testing. Fixed with a single process-wide
   `threading.Lock()` (`app/services/netcdf_lock.py`) that every
   `netCDF4.Dataset(...)` open/read/close anywhere in the project must
   hold — see that module's docstring. Compounding it: reading a
   reflectance band's full-resolution array before downsampling (fine for
   the 2km bands 9/13, ~118MB) is multiple GB for Band 2 at its native
   0.5km (~21700×21700px full disk) — enough to reproduce an OOM kill on
   this project's ~4GB deployment host with two composites running at
   once. Fixed by reading a strided (already-downsampled) view straight
   from the netCDF variable (`_read_source_downsampled()`) for the
   composite products instead of materializing the full array first. If
   you add another product that reads Band 1/2/3/4/6 at anything near
   full resolution, use `_read_source_downsampled()`, not `_read_source()`.

### Roadmap (not yet implemented)

1. **Standalone Band 2 (visible) as its own product**, and **bbox (`center`/
   `dims`) support for the sandwich/geocolor composites** — both composites
   are full-disk only today (see `app/services/goes.py`'s
   `render_sandwich_to_png`/`render_geocolor_to_png`); cropping would need
   each companion band to go through the same locate-then-crop logic
   `render_bbox_to_png` already has for single bands.
2. **A closer-to-official GeoColor** — today's `product=geocolor` is a
   documented approximation (synthetic true color + colorized IR, blended
   by solar zenith angle; see `GET /v1/satellite/products`). No city-lights
   layer, no atmospheric/Rayleigh correction. Closing that gap means
   sourcing (or building) a city-lights raster and implementing real
   atmospheric correction — a substantially bigger lift than the rest of
   this project's rendering pipeline.
3. **TDR**: see `app/services/tdr.py` docstring. In short: crawl
   `https://seb.omao.noaa.gov/pub/acdata/{year}/` for `YYYYMMDD[N|I|H]#/`
   mission directories (no manifest exists — build a local index, e.g.
   SQLite), download/extract the `.tar.gz` bundles in each mission's
   `RADAR_TDR/`, parse the raw netCDF sweeps (variable/dimension layout not
   yet inspected from a real file as of this writing), and render to the
   same storm-relative grid + Plotly-colorscale shape the hurricanes site's
   `js/tdr-archive.js` already consumes from TC-Atlas (match that response
   shape so the client needs minimal changes when migrated onto this API).
4. **Raw netCDF passthrough** (`app/routers/raw.py`): for the GOES side this
   can subset directly from the same file `goes.py` already downloads (no new
   data source) — implement as a `netCDF4` variable slice by
   center/dimensions, streamed back with `Content-Type: application/x-netcdf`.
   The recon MET side already has this — see `GET /v1/recon/mission/{id}/download`.
   The TDR side depends on (3) above.
5. **Migrate the hurricanes site's `goes-archive.js`/`tdr-archive.js`** onto
   this API instead of the local `goes_tile.py` subprocess / TC-Atlas proxy
   — not done in the MVP since those already work in production; this is a
   deliberate follow-up, not an oversight. (`recon-archive.js` and the storm
   track archive have already been migrated onto this API.)
6. Move off this host into its own container — `Dockerfile`/
   `docker-compose.yml` already exist for this.
7. **Extend historical satellite coverage** — see the note in API.md /
   the project's GOES satellite history research; pre-2017 storms (e.g.
   Katrina, 2005) predate the ABI instrument entirely and need a
   different data source and file-format parser, not just another S3
   bucket.

### Conventions to keep

- CORS stays open (`allow_origins=["*"]`) — this is meant for third-party
  sites, not just the hurricanes site.
- New endpoints should return the same `{status, ...}` shape pattern as
  `satellite.py` (`ready|generating|error|idle`) for anything that does
  background work, so polling clients have one contract to handle.
- Keep dependencies minimal — no rasterio/pyproj/boto3/satpy/metpy, matching
  the constraint the original `goes_tile.py` was built under (plain
  `netCDF4`/`numpy`/`Pillow`/stdlib + `httpx` for async HTTP).
- Output rows must stay spaced in Web Mercator (`_mercator_y`), not plain
  latitude — see bug #4 above. Any new render path needs the same spacing.

## License

MIT — see `LICENSE`.
