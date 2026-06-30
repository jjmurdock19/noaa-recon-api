# API Reference

Full endpoint reference and integration examples for `noaa-recon-api`. This
file is meant to be readable both by humans skimming for the right endpoint
and by AI agents integrating the API into another codebase — every example
is copy-pasteable and every response shape is shown verbatim from a real
request. For a terser, agent-optimized summary, see [`llms.txt`](llms.txt).

**Base URLs**

| Environment | Base URL |
|---|---|
| Live (production) | `https://joshmurdock.net/api` |
| Local dev | `http://127.0.0.1:8000` |

**Live interactive docs (always in sync with the code, since FastAPI
generates them from the route definitions):**
- Swagger UI: `{base}/docs`
- OpenAPI schema (machine-readable, for codegen/agent tooling): `{base}/openapi.json`

In production, the API is reverse-proxied behind a path prefix
(`joshmurdock.net/api/` → this app's `/`). FastAPI's auto-generated
Swagger UI doesn't know about that prefix unless told — without it, `/docs`
tries to fetch `openapi.json` from the domain root and 404s. Fixed by
starting uvicorn with `--root-path /api` in production only (see
`deploy/noaa-recon-api.service`); local dev (`uvicorn app.main:app
--reload`, no `--root-path`) is unaffected since there's no prefix there.
If you ever see "Failed to load API definition" / `404 /openapi.json` in
Swagger UI again, this is the first thing to check — same root cause
class as any "absolute path resolves to the wrong place behind a reverse
proxy" bug (see the admin console's `API_BASE` pattern in
`app/console/index.html` for the client-side equivalent fix).

---

## Status legend

| | Meaning |
|---|---|
| 🟢 Live | Implemented, tested against live NOAA data, safe to integrate against today |
| 🟡 Planned | Returns `501 Not Implemented` with a message; shape documented below for forward compatibility |

| Endpoint | Status |
|---|---|
| `GET /v1/health` | 🟢 Live |
| `GET /v1/satellite/tile` (GOES Band 13 / 9) | 🟢 Live |
| `GET /v1/satellite/status/{key}` | 🟢 Live |
| `GET /v1/satellite/colortable` | 🟢 Live |
| `GET /v1/satellite/tile` (Band 2 / GeoColor) | 🟡 Planned |
| `GET /v1/tdr/missions` | 🟡 Planned |
| `GET /v1/tdr/sweep` | 🟡 Planned |
| `GET /v1/raw/netcdf` | 🟡 Planned |
| `GET /demo/netcdf-three/` (static 3D client) | 🟢 Live (sample data only until raw passthrough ships) |
| `GET /` (admin console) + `/v1/admin/*` | 🟢 Live (login required, see README) |

---

## `GET /v1/health`

```bash
curl https://joshmurdock.net/api/v1/health
# {"status":"ok"}
```

---

## `GET /v1/satellite/tile` 🟢

Requests a georeferenced GOES IR/WV tile for an arbitrary UTC timestamp. The
API resolves the **nearest actual ABI scan** to your timestamp (full-disk
scans land roughly every 10 minutes) — you don't need to know exact scan
times in advance. First request for a given scan downloads ~25MB from NOAA
S3 and takes 30–90s; the result is then cached and any repeat request
(including by other clients) returns instantly.

This is an async job: the first call kicks off rendering and returns
`status: "generating"`; poll [`/v1/satellite/status/{key}`](#get-v1satellitestatuskey)
until `status` becomes `"ready"` or `"error"`.

### Query parameters

| Param | Type | Default | Notes |
|---|---|---|---|
| `time` | ISO 8601 UTC datetime | *required* | e.g. `2024-09-28T12:00:00Z`. Resolved to the nearest available scan. |
| `band` | int | `13` | `13` = Clean IR (10.3µm), `9` = Water Vapor (6.9µm). Band 2 (visible) and GeoColor are planned, not yet accepted. |
| `cmap` | string | `default` | One of `default`, `abi13`, `abi9`, `bd`, `ir4`, `enhanced`, `nrl`, `grayscale` — see color tables below. |
| `satellite` | string | `goes-east` | `goes-east` (auto-resolves GOES-16 vs GOES-19 by date) or `goes-west` (auto-resolves GOES-17 vs GOES-18 by date). Both only cover the ABI era (~2017-2018 onward) — see "Satellite coverage" below. |
| `center` | string | *(none)* | `"lat,lon"`, e.g. `"25.5,-80.3"`. Renders a box around this point instead of the full disk — much faster and higher detail (see below). Requires `dims`. |
| `dims` | float | *(none)* | Full width/height of the box centered on `center` (a square box). Requires `center`. Clamped to 10–8000km. |
| `unit` | string | `nm` | Unit for `dims`: `nm` (nautical miles) or `km`. |
| `resolution_km` | float | *(native)* | km per output pixel for a bbox request. Omit for the sensor's native resolution (highest detail — 2km for bands 9/13 today). Increase to render faster / produce a smaller file; can't go finer than native (silently clamped up). |

### Color tables (`cmap`)

`default` is recommended for almost all use — it resolves server-side to
the correct **per-band** standard enhancement (`abi13` for `band=13`,
`abi9` for `band=9`). Band 13 (IR window) and Band 9 (water vapor) measure
different physical quantities and use genuinely different color
conventions — there is no single colortable that's correct for both, so
`default` is band-aware rather than a fixed choice.

| Value | Description |
|---|---|
| `default` | Resolves to `abi13` or `abi9` based on `band` (see below). |
| `abi13` | **Band 13 standard enhancement.** White at the most extreme cold overshooting tops (-110°C) down through black (-80°C), a rainbow band from -80°C to -32°C highlighting severe convection, a hard cut to light grey at -31°C, then greyscale (light=cold, dark=warm) to black at +57°C — most scenes are mostly greyscale, with color only appearing over genuinely severe convection. Exact temperature→hex stops, not an approximation — see `_ABI13_STOPS` in `app/services/goes.py`. |
| `abi9` | **Band 9 (water vapor) standard enhancement.** Cyan at coldest/moist (-93°C) through green tones, white at the moist/dry transition (-42°C), a purple/navy/indigo band (-30°C to -18°C), then yellow→orange→red to black at warmest/driest (+7°C). Exact temperature→hex stops, not an approximation — see `_ABI9_STOPS` in `app/services/goes.py`. Do not use this for Band 13 (or vice versa) — it represents a different physical quantity. |
| `ir4` | An alternate Band 13 enhancement sourced verbatim from [satpy](https://github.com/pytroll/satpy)'s `colorized_ir_clouds` enhancement: greyscale -20°C to +30°C, then the [ColorBrewer "Spectral"](https://colorbrewer2.org) 11-class diverging palette -80°C to -20°C. Kept for comparison; `abi13` is the recommended default for Band 13. |
| `bd` | Standard NWS/Dvorak BD enhancement — greyscale for warm/moderate tops, blue→purple→red for cold convection |
| `enhanced` | Darker surface/low clouds, white mid/high clouds, color for coldest tops |
| `nrl` | Naval Research Lab tropical cyclone enhancement — smooth yellow-green→cyan→blue→purple→red ramp |
| `grayscale` | Plain linear greyscale by brightness temperature |

### Satellite coverage (`satellite`)

| Satellite | Bucket | Active dates |
|---|---|---|
| GOES-16 (East) | `noaa-goes16` | until 2025-01-14 |
| GOES-19 (East) | `noaa-goes19` | 2025-01-14 onward |
| GOES-17 (West) | `noaa-goes17` | until 2023-01-10 (earliest data ~2018) |
| GOES-18 (West) | `noaa-goes18` | 2023-01-10 onward |

`satellite=goes-east`/`goes-west` auto-resolve to the right satellite for
the requested `time`; you don't need to track these cutover dates
yourself.

**This only reaches the ABI era (~2017-2018 onward).** Older storms (e.g.
Hurricane Katrina, 2005) were observed by earlier GOES satellites (GOES-12
at the time) using a completely different instrument — not ABI, no open S3
archive. The only access path for that era is NOAA CLASS/NCEI's
order-based system (you submit a request, wait — hours to weeks — and get
a download link that **expires in 96 hours**), which is fundamentally
incompatible with this API's on-demand design, and the data format isn't
netCDF4/ABI-L2-CMIPF, so it would need its own parser. Not implemented;
see the README roadmap if you want to scope that as a separate project.

### Bounding-box requests (`center` + `dims`)

By default the API renders the **full disk** (~162° across), downsampled
for a manageable file size — fine for an overview, but slow (10-15s to
process) and low-detail for a specific storm. Passing `center` + `dims`
instead renders **only that area, at up to the sensor's native ~2km
resolution** — both meaningfully faster to process and far more detailed.
Measured on this deployment: a 500km box at native resolution processes in
**~1.3s vs. ~14s for a full-disk render** (image data only — the initial
~25MB NOAA S3 download is unaffected either way and is the dominant cost
on a cold cache), and produces a **~130x smaller PNG** (tens of KB instead
of several MB).

```bash
curl "https://joshmurdock.net/api/v1/satellite/tile?time=2024-09-28T12:00:00Z&band=13&center=25.7617,-80.1918&dims=270&unit=nm"
```

### Example request (full disk)

```bash
curl "https://joshmurdock.net/api/v1/satellite/tile?time=2024-09-28T12:00:00Z&band=13"
```

First call (job started):
```json
{"status": "generating", "key": "goes_13_abi13_16_20240928T115621"}
```

After polling to completion:
```json
{
  "status": "ready",
  "key": "goes_13_abi13_16_20240928T115621",
  "png_url": "/cache/satellite/goes_13_abi13_16_20240928T115621.png",
  "bounds": [[-81.3, -156.0], [81.3, 6.0]],
  "band": 13,
  "cmap": "abi13",
  "satellite": "GOES-16",
  "sat_lon": -75.0,
  "scan_start": "2024-09-28T11:56:21+00:00",
  "center": null,
  "width_km": null,
  "resolution_km": null
}
```

Note `cmap` in the response is the **resolved** table (`abi13`), not the
literal `default` you requested — always read it back from the response
rather than assuming.

A bbox request's `ready` response additionally has `center` (`[lat, lon]`),
`width_km` (the resolved box size — note `dims`/`unit` get converted to km),
and `resolution_km` (the resolved render resolution, native unless you
overrode it) populated instead of `null`.

- `png_url` is **relative to the API's own base URL**, not the page you're
  integrating into — prefix it yourself: `base + png_url`. For a full-disk
  request it's a 2048×2048 RGBA PNG; for a bbox request its size is
  `width_km / resolution_km` pixels (clamped to 64–4096). Transparent
  pixels are off-disk/no-data either way.
- `bounds` is `[[lat_south, lon_west], [lat_north, lon_east]]` — the exact
  format `L.imageOverlay(url, bounds)` expects in Leaflet.
- `scan_start` is the **actual** scan time used, which may differ from your
  requested `time` by a few minutes (nearest-match).

### Error response

```json
{"status": "error", "key": "...", "message": "No GOES Band 13 scan found near ..."}
```

A bbox request can also fail with messages like `"Requested area is outside
this scan's visible disk"` (point not on the half of Earth this satellite
sees) — `center`/`dims` are still validated (lat/lon range, box size
bounds) before the scan is even resolved, so malformed requests fail fast
with `400` rather than waiting on a render.

---

## `GET /v1/satellite/status/{key}` 🟢

Poll this with the `key` returned above.

```bash
curl https://joshmurdock.net/api/v1/satellite/status/goes_13_bd_16_20240928T115621
```

| `status` | Meaning |
|---|---|
| `ready` | Done — full metadata included (see shape above) |
| `generating` | Still rendering — includes `elapsed` (seconds) |
| `error` | Failed — includes `message` |
| `idle` | Unknown key (never requested, or cache expired) |

Suggested poll interval: 3–5s.

---

## `GET /v1/satellite/colortable` 🟢

Returns the exact color stops for a colortable, so a client can render a
legend that's **guaranteed to match** what `/tile` actually produces — it
reads the same `STOPS_BY_CMAP`/`LUTS` data the renderer uses, not a
hardcoded copy that could drift out of sync.

| Param | Type | Default | Notes |
|---|---|---|---|
| `cmap` | string | `default` | Same values as `/tile`'s `cmap`. |
| `band` | int | `13` | Only used to resolve `cmap=default`. |

```bash
curl "https://joshmurdock.net/api/v1/satellite/colortable?cmap=default&band=13"
```

```json
{
  "cmap": "abi13",
  "unit": "C",
  "exact": true,
  "stops": [
    {"temp_c": -110, "hex": "#FFFFFF"},
    {"temp_c": -80, "hex": "#000000"},
    {"temp_c": -75, "hex": "#330000"},
    ...
    {"temp_c": 57, "hex": "#000000"}
  ]
}
```

- `exact: true` for `abi13`/`abi9` — every stop is the literal source data
  (see "A real bug already found and fixed here" below). `exact: false`
  for the other (LUT-based) colortables, where `stops` is a representative
  16-point sample rather than every value.
- To render a CSS gradient legend: sort `stops` ascending by `temp_c`
  (already sorted), map each to a percentage position
  `(temp_c - min) / (max - min) * 100`, and build
  `linear-gradient(to right, hex1 pct1%, hex2 pct2%, ...)`. This is
  exactly what the hurricanes site's `js/api-explorer.js` does (see
  `_renderLegend()`).

---

## `GET /v1/tdr/missions` 🟡 / `GET /v1/tdr/sweep` 🟡

Not implemented yet — both return `501` today. Planned shape (subject to
change until implemented): `sweep` will mirror the response shape the
hurricanes site's `js/tdr-archive.js` already consumes from a third-party
API (TC-Atlas) — a storm-relative grid (`x`/`y` in km from storm center),
a `data` 2D array, and a Plotly-style `colorscale` — so the same client
rendering code can be pointed at this API once it ships. See
[`app/services/tdr.py`](app/services/tdr.py) for the in-progress design
(mission crawler over NOAA's raw archive, no manifest exists today so a
local index has to be built).

---

## `GET /v1/raw/netcdf` 🟡

Not implemented yet — returns `501`. Planned: server-side `netCDF4`
variable subsetting by `data_type` (`satellite`|`tdr`), `band`/`variable`,
`time`, `center` (lat,lon), and `dims` (km), streamed back as
`Content-Type: application/x-netcdf` for client-side rendering (e.g. via
[`netcdf-three`](https://github.com/umrlastig/netcdf-three) — see the demo
client below). See [`app/routers/raw.py`](app/routers/raw.py).

---

## `GET /demo/netcdf-three/` 🟢

A static page (no build step) demonstrating client-side netCDF rendering
with Three.js/WebGL via the vendored `netcdf-three` library. Currently
loads that library's bundled sample dataset (a 3D volume) since the live
raw-passthrough endpoint above isn't implemented yet — swap the
`DATA_URL` constant in `clients/netcdf-three-demo/index.html` once it is.

```
https://joshmurdock.net/api/demo/netcdf-three/
```

---

## `GET /` — Admin console 🟢

A login-gated web UI for operating this deployment: cache status/storage
stats, browsing and deleting cached rendered tiles and raw netCDF
downloads, submitting one-off queries, and bulk-prefetching a timeframe
into the cache. Static page at `app/console/index.html`, calling the
`/v1/admin/*` JSON endpoints below. See the README's "Admin console"
section for the credentials file (`admin_credentials.json`, gitignored,
default `admin`/`password` — change it before exposing this publicly).

```
https://joshmurdock.net/api/
```

### `/v1/admin/*` endpoints (all require a logged-in session except login/whoami)

| Endpoint | Method | Purpose |
|---|---|---|
| `/v1/admin/login` | POST | `{username, password}` JSON body → sets session cookie or `401`. |
| `/v1/admin/logout` | POST | Clears the session. |
| `/v1/admin/whoami` | GET | `{authenticated: bool}` — no login required, used by the console to decide whether to show the login form. |
| `/v1/admin/status` | GET | Cache stats: file count + bytes for `satellite` (rendered tiles) and `goes_nc` (raw downloads) separately, plus a total. |
| `/v1/admin/cache/satellite` | GET | List every cached rendered-tile entry — **every field the render pipeline wrote** (key, status, band, cmap, satellite, sat_lon, scan_start, bounds, center, width_km, resolution_km, png_url, size, modified), not a curated subset, so the console's preview pane has everything without a second round-trip. |
| `/v1/admin/cache/satellite/{key}` | DELETE | Delete one entry's `.png`/`.json`/`.lock` files. |
| `/v1/admin/cache/satellite` | DELETE | Delete all rendered-tile cache entries. |
| `/v1/admin/cache/goes_nc` | GET | List every cached raw netCDF download (filename, parsed scan time, size, modified). |
| `/v1/admin/cache/goes_nc/{filename}/info` | GET | Structural metadata for one raw netCDF file — dimensions, every variable (name/dims/shape/dtype/units/long_name), and global attributes. Analogous to `ncdump -h`; this is how the console "previews" a netCDF file since it isn't directly viewable like an image. |
| `/v1/admin/cache/goes_nc/{filename}` | DELETE | Delete one raw netCDF file. |
| `/v1/admin/cache/goes_nc` | DELETE | Delete all raw netCDF downloads (next requests re-download from NOAA S3). |
| `/v1/admin/prefetch` | POST | Bulk-load a timeframe into cache — see below. Returns a job immediately; poll for progress. |
| `/v1/admin/prefetch/{job_id}` | GET | Poll a prefetch job's progress. |
| `/v1/admin/prefetch` | GET | List all prefetch jobs (in-memory — lost on process restart). |

### Bulk prefetch

```json
POST /v1/admin/prefetch
{
  "time_start": "2025-10-28T06:00:00Z",
  "time_end": "2025-10-28T18:00:00Z",
  "interval_minutes": 30,
  "band": 13,
  "satellite": "goes-east",
  "cmap": "default",
  "center": "17.55,-78.14",
  "dims": 1000,
  "unit": "nm"
}
```

`center`/`dims`/`unit` are optional (omit for full-disk prefetch — much
slower per frame, see the bbox section above). Capped at 500 generated
timestamps per job (400 if you ask for more) to prevent an accidental
multi-hour job; lower `interval_minutes` or shorten the range. Each
timestamp goes through the exact same `resolve_nearest`/`render_and_store`
pipeline as a normal `/tile` request — prefetched entries are
indistinguishable from organically-requested ones in the cache, and a
timestamp already cached as `ready` is skipped (counted separately from
`completed`), not re-rendered.

---

## Logging

Every request gets one line in `logs/app.log` (method, path, status code,
duration in ms, client IP) — see `app/main.py`'s `log_requests` middleware
— plus any `log.exception(...)` calls already in the codebase (e.g.
render failures in `app/services/goes.py`) and an automatic log line for
any unhandled exception, with full traceback, before it's re-raised as a
500. The file rotates at 10MB with 5 backups kept (`app/logging_config.py`)
so it doesn't grow unbounded — this is for ongoing/long-term monitoring,
not just whatever stdout the process happens to be attached to.

```bash
tail -f logs/app.log
```

`logs/` is gitignored (same as `cache/`) — it's host-local state, not
repo content.

---

## Integration examples

### curl — request + poll loop

```bash
KEY=$(curl -s "https://joshmurdock.net/api/v1/satellite/tile?time=$(date -u +%FT%TZ)&band=13" | python3 -c "import json,sys; print(json.load(sys.stdin)['key'])")
until curl -s "https://joshmurdock.net/api/v1/satellite/status/$KEY" | grep -q '"status":"ready"'; do sleep 3; done
curl -s "https://joshmurdock.net/api/v1/satellite/status/$KEY"
```

### JavaScript — overlay on a Leaflet map

```javascript
const API_BASE = 'https://joshmurdock.net/api';

// Pass { center: '25.5,-80.3', dims: 270, unit: 'nm' } to render a fast,
// high-detail box instead of the full disk — see "Bounding-box requests" above.
async function loadGoesTile(map, { time, band = 13, cmap = 'default', center, dims, unit, resolution_km }) {
  const params = { time, band, cmap };
  if (center) Object.assign(params, { center, dims, unit: unit || 'nm' });
  if (resolution_km) params.resolution_km = resolution_km;

  let data = await fetch(`${API_BASE}/v1/satellite/tile?${new URLSearchParams(params)}`).then(r => r.json());

  while (data.status === 'generating') {
    await new Promise(res => setTimeout(res, 3000));
    data = await fetch(`${API_BASE}/v1/satellite/status/${data.key}`).then(r => r.json());
  }
  if (data.status !== 'ready') throw new Error(data.message || 'tile render failed');

  return L.imageOverlay(API_BASE + data.png_url, data.bounds, { opacity: 0.85 }).addTo(map);
}

// Full disk:      loadGoesTile(map, { time: new Date().toISOString() });
// Fast regional:  loadGoesTile(map, { time: new Date().toISOString(), center: '25.7617,-80.1918', dims: 270, unit: 'nm' });
```

(This is exactly the pattern used in the hurricanes tracker site's `js/api-explorer.js` — see that file for the full version with status-polling UI, the click-on-map point picker, band/colormap pickers, etc.)

### Python — fetch and save the PNG locally

```python
import time, requests

API_BASE = "https://joshmurdock.net/api"

def fetch_goes_tile(time_iso, band=13, cmap="default"):
    r = requests.get(f"{API_BASE}/v1/satellite/tile",
                      params={"time": time_iso, "band": band, "cmap": cmap})
    data = r.json()
    while data["status"] == "generating":
        time.sleep(3)
        data = requests.get(f"{API_BASE}/v1/satellite/status/{data['key']}").json()
    if data["status"] != "ready":
        raise RuntimeError(data.get("message", "render failed"))
    png = requests.get(API_BASE + data["png_url"]).content
    with open(f"{data['key']}.png", "wb") as f:
        f.write(png)
    return data

# fetch_goes_tile("2024-09-28T12:00:00Z")
```

---

## Notes for agents integrating this API

- All responses are JSON except the PNG tiles themselves (`image/png`) and
  `/v1/raw/netcdf` once implemented (`application/x-netcdf`).
- CORS is open (`Access-Control-Allow-Origin: *`) — this API is designed to
  be called directly from a browser on any origin, no proxy required.
- No authentication, no API key, no rate limiting is currently enforced.
  Be a good citizen: cache results client-side where you can, and avoid
  polling faster than every 3s.
- The `key` returned by `/v1/satellite/tile` is deterministic for a given
  `(band, cmap, satellite, resolved-scan-time)` (plus `center`/`width_km`/
  `resolution_km` for a bbox request) — repeated identical requests are
  cheap (cache hit), not re-rendered.
- Prefer `center`+`dims` over a full-disk request whenever you know roughly
  where you need imagery (e.g. you already have a storm's lat/lon) — it's
  both faster to process and much higher detail. The first request for a
  given *scan* still has to download ~25MB from NOAA S3 regardless of bbox
  vs. full-disk (that part isn't optimized yet); the bbox speedup is in the
  reprojection/render step and the output file size.
- `resolution_km` can't go finer than the sensor's native pixel size (2km
  for bands 9/13 today) — requesting finer is silently clamped up to native
  rather than erroring, so it's safe to pass an optimistic value.
- The OpenAPI schema at `{base}/openapi.json` is generated directly from
  the route definitions and is the most authoritative machine-readable
  source if this document drifts.
