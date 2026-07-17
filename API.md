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
| `GET /v1/satellite/tile` (bands 1-16) | 🟢 Live |
| `GET /v1/satellite/tile` (`product=sandwich`, `product=geocolor`) | 🟢 Live (geocolor is an approximation — see `/v1/satellite/products`) |
| `GET /v1/satellite/status/{key}` | 🟢 Live |
| `GET /v1/satellite/colortable` | 🟢 Live |
| `GET /v1/satellite/colortables` | 🟢 Live |
| `GET /v1/satellite/products` | 🟢 Live |
| `GET /v1/storms/years` | 🟢 Live |
| `GET /v1/storms/{year}` | 🟢 Live |
| `GET /v1/storms/{year}/{name}` | 🟢 Live |
| `GET /v1/storms/{year}/{name}/nearest` | 🟢 Live |
| `GET /v1/recon/years` | 🟢 Live |
| `GET /v1/recon/{year}` | 🟢 Live |
| `GET /v1/recon/{year}/{storm_name}` | 🟢 Live |
| `GET /v1/recon/mission/{mission_id}` | 🟢 Live |
| `GET /v1/recon/mission/{mission_id}/download` | 🟢 Live |
| `GET /v1/tdr/years` | 🟢 Live |
| `GET /v1/tdr/{year}` | 🟢 Live |
| `GET /v1/tdr/{year}/{storm_name}` | 🟢 Live |
| `GET /v1/tdr/mission/{mission_id}` | 🟢 Live |
| `GET /v1/tdr/sweep` | 🟢 Live (post-2021 Cartesian-grid files only — see below) |
| `GET /v1/raw/netcdf` | 🟡 Planned |
| `GET /demo/netcdf-three/` (static 3D client) | 🟢 Live (sample data only until raw passthrough ships) |
| `GET /` (admin console) + `/v1/admin/*` | 🟢 Live (login required, see README) |

---

## Authentication

Off by default — every endpoint below behaves exactly as documented, no
setup required. A deployer can opt into requiring an API token for the
public data endpoints (e.g. to track or restrict usage on their own
instance); check `GET /v1/health` or ask the operator if you're not sure
whether the instance you're calling has it turned on.

When enabled:

- **Every `/v1/*` data endpoint** (satellite, storms, recon, tdr, raw)
  requires `Authorization: Bearer <token>`. An expired/missing/revoked
  token gets a `401`.
- **`GET /v1/health`** and the admin console's own login screen stay
  reachable without a token either way — health checks and "is this API
  up" monitoring shouldn't depend on having a key.
- **Cached tile URLs stay unauthenticated by design.** A satellite tile
  request returns a `png_url` under `/cache/...` once rendered (see
  `GET /v1/satellite/status/{key}`) — that URL gets embedded directly in
  an `<img>` tag or a Leaflet `imageOverlay`, neither of which can send a
  custom header. The token gate only applies to the JSON endpoint that
  *starts* a render; its resulting cache key isn't guessable without
  already having called that gated endpoint.
- Any token works regardless of role — a superuser/moderator's own token
  (the same one that doubles as part of their console login) is just as
  valid an `Authorization: Bearer` value as a plain API key issued to a
  third party.

Get a token from whoever runs the instance (admin console → API
management → Tokens). There is no self-service signup.

```bash
curl -H "Authorization: Bearer $TOKEN" \
  "https://joshmurdock.net/api/v1/satellite/tile?time=2024-09-28T12:00:00Z&band=13"
```

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
| `band` | int | `13` | `13` = Clean IR (10.3µm), `9` = Water Vapor (6.9µm), `7` = Shortwave IR / "fire temperature" (3.9µm), `5` = Near-IR Snow/Ice (1.6µm, reflectance), `3` = Veggie / Vegetation-NIR (0.86µm, reflectance), `2` = Red / Visible (0.64µm, reflectance). Ignored if `product` is given. |
| `cmap` | string | `default` | One of `default`, `abi13`, `abi9`, `abi7`, `abi5`, `abi3`, `abi2`, `bd`, `ir4`, `enhanced`, `nrl`, `grayscale` — see color tables below. Ignored if `product` is given. |
| `product` | string | *(none)* | `sandwich` or `geocolor` — a multi-band composite (see below). When given, `band`/`cmap` are ignored. `center`/`dims` (bbox) are supported the same as a single-band tile. |
| `satellite` | string | `goes-east` | `goes-east` (auto-resolves GOES-16 vs GOES-19 by date) or `goes-west` (auto-resolves GOES-17 vs GOES-18 by date). Both only cover the ABI era (~2017-2019 onward) — see "Satellite coverage" below. |
| `center` | string | *(none)* | `"lat,lon"`, e.g. `"25.5,-80.3"`. Renders a box around this point instead of the full disk — much faster and higher detail (see below). Requires `dims`. Works with `product` too. |
| `dims` | float | *(none)* | Full width/height of the box centered on `center` (a square box). Requires `center`. Clamped to 10–8000km. |
| `unit` | string | `nm` | Unit for `dims`: `nm` (nautical miles) or `km`. |
| `resolution_km` | float | *(native)* | km per output pixel for a bbox request. Omit for the sensor's native resolution (highest detail — 2km for most bands, 1km for bands 3/5, 0.5km for band 2). Increase to render faster / produce a smaller file; can't go finer than native (silently clamped up). |

### Color tables (`cmap`)

`default` is recommended for almost all use — it resolves server-side to
the correct **per-band** standard enhancement. Every band measures a
different physical quantity (brightness temperature for 7/9/13,
reflectance for 2/3/5) and uses a genuinely different color convention —
there is no single colortable that's correct for all of them, so
`default` is band-aware rather than a fixed choice.

| Value | Description |
|---|---|
| `default` | Resolves to `abi13`/`abi9`/`abi7`/`abi5`/`abi3`/`abi2` based on `band` (see below). |
| `abi13` | **Band 13 standard enhancement.** White at the most extreme cold overshooting tops (-110°C) down through black (-80°C), a rainbow band from -80°C to -32°C highlighting severe convection, a hard cut to light grey at -31°C, then greyscale (light=cold, dark=warm) to black at +57°C — most scenes are mostly greyscale, with color only appearing over genuinely severe convection. Exact temperature→hex stops, not an approximation — see `_ABI13_STOPS` in `app/services/goes.py`. |
| `abi9` | **Band 9 (water vapor) standard enhancement.** Cyan at coldest/moist (-93°C) through green tones, white at the moist/dry transition (-42°C), a purple/navy/indigo band (-30°C to -18°C), then yellow→orange→red to black at warmest/driest (+7°C). Exact temperature→hex stops, not an approximation — see `_ABI9_STOPS` in `app/services/goes.py`. Do not use this for Band 13 (or vice versa) — it represents a different physical quantity. |
| `abi7` | **Band 7 (shortwave IR, "fire temperature") standard enhancement.** Greyscale over the same cloud-top range as 9/13, then a yellow→red highlight above normal clear-sky warmth (~+57°C) to flag hotspots — this band saturates far higher than 9/13 (fires can push 400K+), inspired by (not identical to) common operational SWIR/fire-temperature displays. See `_ABI7_STOPS` in `app/services/goes.py`. |
| `abi5` | **Band 5 (near-IR snow/ice) reflectance ramp.** Not a temperature colortable — Band 5 reports reflectance factor (~0-1), rendered as a gamma-stretched 0-100% grayscale (linear reflectance reads unnaturally flat/dark to the eye). See `_reflectance_gray()` in `app/services/goes.py`. |
| `abi3` | **Band 3 ("Veggie", vegetation/near-IR) reflectance ramp.** Same treatment as `abi5` — reflectance, not temperature, rendered as a gamma-stretched grayscale via `_reflectance_gray()`. Sensitive to chlorophyll/vegetation reflectance, hence the nickname (this is NOAA's own name for the band, not this project's). |
| `abi2` | **Band 2 (red/visible) reflectance ramp.** Same treatment as `abi5`/`abi3` — reflectance, not temperature, rendered as a gamma-stretched grayscale via `_reflectance_gray()`. The sharpest band this API renders (0.5km native) — daylight-only, no signal at night. Also the visible input to `product=sandwich`. |
| `ir4` | An alternate Band 13 enhancement sourced verbatim from [satpy](https://github.com/pytroll/satpy)'s `colorized_ir_clouds` enhancement: greyscale -20°C to +30°C, then the [ColorBrewer "Spectral"](https://colorbrewer2.org) 11-class diverging palette -80°C to -20°C. Kept for comparison; `abi13` is the recommended default for Band 13. |
| `bd` | Standard NWS/Dvorak BD enhancement — greyscale for warm/moderate tops, blue→purple→red for cold convection |
| `enhanced` | Darker surface/low clouds, white mid/high clouds, color for coldest tops |
| `nrl` | Naval Research Lab tropical cyclone enhancement — smooth yellow-green→cyan→blue→purple→red ramp |
| `grayscale` | Plain linear greyscale by brightness temperature |

`bd`/`enhanced`/`nrl`/`grayscale`/`ir4` were tuned for bands 9/13's
-113..+42°C range — applying them to Band 7 works (its brightness
temperature is on the same Kelvin scale) but clips anything above +42°C
to the same color, silently losing Band 7's fire-highlighting range.
Use `abi7` (the default) if that matters.

### Composite products (`product`)

| Value | Description |
|---|---|
| `sandwich` | Band 13 IR (the `abi13` enhancement) modulated by Band 2 visible brightness, to show convective texture (overshooting tops, gravity waves) a pure IR colorization smooths over. Falls back to a darkened plain-IR look at night (no visible signal to show texture from). |
| `geocolor` | A **simplified approximation** of NOAA/CIRA's GeoColor — day side: synthetic true color from Bands 1/2/3 (green is synthesized via `0.45*red + 0.10*NIR + 0.45*blue`, CIRA's published recipe, since ABI has no native green channel); night side: `abi13` colorized IR; blended by solar zenith angle across the terminator. **Not** the official product: no city-lights layer, no atmospheric/Rayleigh correction. See `GET /v1/satellite/products` for the exact same description in machine-readable form. |

Both composites fetch every companion band from the *exact same scan
cycle* (ABI captures all bands simultaneously per scan, so there's no
time-misalignment between e.g. the IR and visible channels). `center`/
`dims` (bbox) work the same as a single-band tile — each companion band is
cropped and read directly from its source file rather than reprojecting
the full disk, which matters most for Band 2 (both composites' finest
input, at native 0.5km): materializing that band's full disk first isn't
just slow, it risks exhausting memory on a small deployment host, so the
crop is read straight off the file at a stride instead. If a secondary
band (e.g. Band 1/3 for `geocolor`) has no data at all in a requested box
— possible right at the scan's edge — that composite falls back to its
night-side/no-visible-signal rendering for the whole tile rather than
guessing at partial color.

```bash
curl "https://joshmurdock.net/api/v1/satellite/tile?time=2024-09-28T12:00:00Z&product=sandwich"
curl "https://joshmurdock.net/api/v1/satellite/tile?time=2024-09-28T12:00:00Z&product=geocolor"
```

### Satellite coverage (`satellite`)

| Satellite | Bucket | Active dates |
|---|---|---|
| GOES-16 (East) | `noaa-goes16` | 2017-12-18 until 2025-01-14 |
| GOES-19 (East) | `noaa-goes19` | 2025-01-14 onward |
| GOES-17 (West) | `noaa-goes17` | 2019-02-12 until 2023-01-10 |
| GOES-18 (West) | `noaa-goes18` | 2023-01-10 onward |

`GET /v1/satellite/products` returns these same dates in machine-readable
form (`satellites.goes-east`/`satellites.goes-west`), so a client doesn't
need to hardcode them either.

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

## `GET /v1/satellite/colortables` 🟢

Discovery endpoint: every color table usable with a given `band`, with
human-readable names/descriptions — for building a color-table picker UI
(dropdown/swatch list) without hardcoding this project's cmap catalog
client-side. Complements `/colortable` above, which returns the actual
stops for one cmap at a time (call `/colortable?cmap=<picked>&band=<band>`
once the user picks one from this list to render its legend).

| Param | Type | Default | Notes |
|---|---|---|---|
| `band` | int | `13` | One of `3`, `5`, `7`, `9`, `13`. Mutually exclusive with `product`. |
| `product` | string | *(none)* | `sandwich` or `geocolor` — composites don't accept a `cmap` choice (see `/tile`), so this returns the single fixed enhancement each one uses instead of a picker list. Mutually exclusive with `band`. |

```bash
curl "https://joshmurdock.net/api/v1/satellite/colortables?band=13"
```

```json
{
  "band": 13,
  "kind": "brightness_temp",
  "default_cmap": "abi13",
  "colortables": [
    {"cmap": "abi13", "is_default": true, "kind": "brightness_temp", "unit": "C",
     "name": "Band 13 Standard Enhancement", "description": "White at the most extreme..."},
    {"cmap": "bd", "is_default": false, "kind": "brightness_temp", "unit": "C",
     "name": "NWS/Dvorak BD Enhancement", "description": "..."},
    {"cmap": "enhanced", "...": "..."},
    {"cmap": "grayscale", "...": "..."},
    {"cmap": "ir4", "...": "..."},
    {"cmap": "nrl", "...": "..."}
  ]
}
```

For a reflectance band (`3`/`5`) there's only ever one entry — those bands
have no alternate enhancements, only the single gamma-stretched grayscale
ramp (`kind: "reflectance"`, `unit: "%"`):

```bash
curl "https://joshmurdock.net/api/v1/satellite/colortables?band=5"
# {"band":5,"kind":"reflectance","default_cmap":"abi5",
#  "colortables":[{"cmap":"abi5","is_default":true,"kind":"reflectance","unit":"%","name":"...","description":"..."}]}
```

```bash
curl "https://joshmurdock.net/api/v1/satellite/colortables?product=geocolor"
# {"product":"geocolor","colortables":[{"cmap":"abi13","is_default":true,...}],
#  "note":"Composite products always use the abi13 IR enhancement..."}
```

This is the "list every color table for a product" counterpart to
`/products` above — `/products` tells you *which* cmaps apply to each band
in one bulk response (for a product picker); `/colortables` gives you the
same list scoped to one band, with full names/descriptions attached (for a
color-table picker once a band's already chosen).

---

## `GET /v1/satellite/products` 🟢

Discovery endpoint: every single-band and composite product this API can
render, plus the exact UTC date range each satellite covers — so a client
can build a product picker without hardcoding any of this project's
band/cmap/coverage knowledge.

```bash
curl "https://joshmurdock.net/api/v1/satellite/products"
```

```json
{
  "bands": [
    {"band": 2, "name": "Red (Visible), 0.64µm", "kind": "reflectance",
     "default_cmap": "abi2", "cmaps": ["abi2"], "native_resolution_km": 0.5, "bbox_supported": true},
    {"band": 3, "name": "Veggie (Vegetation/NIR), 0.86µm", "kind": "reflectance",
     "default_cmap": "abi3", "cmaps": ["abi3"], "native_resolution_km": 1.0, "bbox_supported": true},
    {"band": 5, "name": "Near-IR (Snow/Ice), 1.6µm", "kind": "reflectance",
     "default_cmap": "abi5", "cmaps": ["abi5"], "native_resolution_km": 1.0, "bbox_supported": true},
    {"band": 7, "name": "Shortwave IR (\"Fire Temperature\"), 3.9µm", "kind": "brightness_temp",
     "default_cmap": "abi7", "cmaps": ["abi7", "bd", "enhanced", "grayscale", "ir4", "nrl"],
     "native_resolution_km": 2.0, "bbox_supported": true},
    {"band": 9, "...": "..."},
    {"band": 13, "...": "..."}
  ],
  "products": [
    {"product": "sandwich", "name": "IR/VIS Sandwich", "description": "...", "bbox_supported": true},
    {"product": "geocolor", "name": "GeoColor-style composite (approximate)", "description": "...", "bbox_supported": true}
  ],
  "satellites": {
    "goes-east": [
      {"satellite": "GOES-16", "start": "2017-12-18", "end": "2025-01-14"},
      {"satellite": "GOES-19", "start": "2025-01-14", "end": null}
    ],
    "goes-west": [
      {"satellite": "GOES-17", "start": "2019-02-12", "end": "2023-01-10"},
      {"satellite": "GOES-18", "start": "2023-01-10", "end": null}
    ]
  }
}
```

`kind` tells you whether a band's colortables are temperature-based
(`brightness_temp`) or a reflectance ramp (`reflectance`) — relevant if
you're building a legend, since the two need different axis units (°C vs
%). An `end: null` satellite is the current one for that side.

---

## `GET /v1/storms/*` 🟢 — Historical storm tracks

A local best-track database, **named storms only** — year + storm name +
an arbitrary datetime in, the closest actual track fix out. Not backed by
TC-Atlas's TDR metadata — that dataset only has fixes at moments a recon
aircraft happened to fly, which can't answer "what was this storm doing at
3am on some arbitrary date" or cover a storm still in progress. Instead:

- **HURDAT2** — NHC's official reconciled archive (Atlantic since 1950,
  East/Central Pacific since 1950s), the authoritative source but only
  republished once a year, months after each season ends.
- **ATCF b-decks** — NHC's operational best-track feed, updated
  near-real-time all season and archived by season once it closes. Fills
  the gap between HURDAT2's last reconciled year and today, so the
  database stays current through the storm happening right now.

Every unnamed depression/invest is filtered out of both sources (see
`is_real_storm_name()` in [`app/services/storms.py`](app/services/storms.py)),
which as a side effect also drops the 19th/early-20th-century tail —
Atlantic storms didn't get real names until 1950.

A nightly systemd timer (`deploy/storm-archive-update.timer`) re-runs
[`scripts/ingest_storms.py`](scripts/ingest_storms.py) to pick up the
latest advisory for any storm currently active; see the README's
"Storm archive updates" section to install it. Every storm upserts by
`atcf_id` and replaces its own track points, so re-running is always safe
whether triggered by the timer or by hand.

| Endpoint | Purpose |
|---|---|
| `GET /v1/storms/years` | Every year with at least one storm on record. |
| `GET /v1/storms/{year}` | Every storm tracked in that year: `{name, basin, atcf_id}[]`. `basin` is `AL` (Atlantic), `EP` (East Pacific), or `CP` (Central Pacific). |
| `GET /v1/storms/{year}/{name}` | Full best-track for one storm — every ~6-hourly fix: `datetime_utc`, `status` (HURDAT2 code), `category` (Saffir-Simpson label for hurricanes, a plain status label otherwise), `lat`/`lon` (decimal degrees, negative = S/W), `wind_kt`, `pressure_mb`. |
| `GET /v1/storms/{year}/{name}/nearest?datetime=...` | The single fix closest in time to an arbitrary ISO 8601 UTC `datetime` — the core "feed a year/name/datetime, get the closest relevant data" lookup. |

Both `{year}/{name}` endpoints accept an optional `?basin=AL\|EP\|CP` to
disambiguate the rare case of the same name reused across basins in the
same year — omitting it returns a `409` listing the candidates if that
happens.

```bash
curl "https://joshmurdock.net/api/v1/storms/2023/LEE/nearest?datetime=2023-09-10T12:00:00Z"
# {"datetime_utc":"2023-09-10T12:00:00Z","status":"HU","category":"Category 2",
#  "lat":21.5,"lon":-60.8,"wind_kt":95,"pressure_mb":956,
#  "year":2023,"name":"LEE","basin":"AL","atcf_id":"AL132023"}
```

---

## `GET /v1/recon/*` 🟢 — Recon MET (1-second flight-level) archive

A local archive of NOAA hurricane hunter aircraft flight-level observation
data — every mission NOAA has flown into a storm since 2011, decimated to
0.2 Hz (every 5th raw 1-second sample) and stored with lat/lon, flight-level
wind, SFMR surface wind, wind direction, and altitude per point. Same
year -> storm -> mission discovery shape as `/v1/storms/*` above, so the
two archives can be cross-referenced (e.g. "show me every recon fix within
an hour of this best-track point") from one API. Crawled from NOAA's raw
archive at `seb.omao.noaa.gov/pub/acdata` — see
[`app/services/recon_met.py`](app/services/recon_met.py) for the crawler
and [`scripts/ingest_recon_met.py`](scripts/ingest_recon_met.py) for the
ingestion entry point (also a nightly systemd timer — see the README's
"Recon MET archive" section).

| Endpoint | Purpose |
|---|---|
| `GET /v1/recon/years` | Every year with at least one archived mission. |
| `GET /v1/recon/{year}` | Every storm with missions that year: `{storm_name, storm_id, mission_count}[]`. Unidentified flights (training, calibration, research) are grouped under `storm_name: "Training / Research"` rather than one bucket per flight. |
| `GET /v1/recon/{year}/{storm_name}` | Every mission for that storm: `{mission_id, aircraft, tail_num, flight_date, start_unix, end_unix, obs_count, source_url}[]`, ordered chronologically. |
| `GET /v1/recon/mission/{mission_id}` | The mission's full decimated track — `mission_id` alone is enough to look it up (unique across every year/storm). Returns metadata plus `obs`: an array of `[unix_time, lat, lon, wind_kt, wind_dir, sfmr_kt, alt_m]` tuples (any field can be `null` if that sensor didn't report). Also includes `source_url`. |
| `GET /v1/recon/mission/{mission_id}/download` | Streams NOAA's original full-resolution NetCDF file for this mission (600+ variables — attitude, airspeed, every raw sensor channel — not just the ~7 fields `/mission/{id}` decimates). Not a redirect: the bytes are proxied through this API with `Content-Type: application/x-netcdf`, since a redirect's success depends on the caller's HTTP client following it, which isn't guaranteed for every netCDF-consuming tool. |

```bash
curl "https://joshmurdock.net/api/v1/recon/2024/Beryl"
# {"year":2024,"storm_name":"Beryl","missions":[
#   {"mission_id":"20240630I1","aircraft":"NOAA 43 (Miss Piggy)","tail_num":"N43",
#    "flight_date":"2024-06-30","start_unix":1719732180,"end_unix":1719765800,
#    "obs_count":6725,"source_url":"https://seb.omao.noaa.gov/pub/acdata/2024/MET/20240630I1/20240630I1_A.nc"},
#   ...]}

curl -o mission.nc "https://joshmurdock.net/api/v1/recon/mission/20240630I1/download"
# streams the full ~85MB original file (632 variables in a typical mission,
# vs. the ~7 this project decimates) — use this if you need anything
# beyond position/wind/SFMR/altitude: attitude (yaw/pitch/roll), airspeed,
# Mach number, multiple GPS/temperature sensor variants, etc.
```

---

## `GET /v1/tdr/*` 🟢 — Tail Doppler Radar (TDR) archive

A local index of NOAA's Tail Doppler Radar mission archive — built the same
way as the recon MET archive (no manifest exists upstream, so
[`crates/server/src/services/tdr_ingest.rs`](crates/server/src/services/tdr_ingest.rs)
crawls the directory listings), but across **two source levels** with
different hosts and QC lineage:

- **Level 1b** — real-time products generated on the aircraft during the
  flight, at `seb.omao.noaa.gov/pub/flight/radar/{mission_id}/`. Available
  as soon as a mission lands; no storm name in the path.
- **Level 2** — the same file shapes, reprocessed on the ground after the
  season with better QC, at
  `www.aoml.noaa.gov/ftp/pub/hrd/data/radar/level2/{year}/{storm_slug}/{mission_id}/`.
  Only available months after a storm, but the storm name is part of the
  path itself.

A mission can appear at either level, both, or (rarely) neither yet.
`mission_id` (`YYYYMMDDAI`) uses the exact same scheme as the recon MET
archive's mission IDs — see "Recon MET archive" above — so storm-name
resolution piggybacks on whatever that archive already reconciled for the
same ID rather than re-deriving it from scratch; a Level 2 directory name is
the fallback, then "Unknown".

This phase only indexes file **metadata** (mission → product → source URL)
— it never downloads a netCDF file. Actual decode/slice rendering
(`GET /v1/tdr/sweep` below) is a follow-up phase.

| Endpoint | Purpose |
|---|---|
| `GET /v1/tdr/years` | Every year with at least one indexed TDR mission. |
| `GET /v1/tdr/{year}` | Every storm with missions that year: `{storm_name, storm_id, mission_count}[]`. Missions not yet reconciled to a storm are grouped under `storm_name: "Unknown"`. |
| `GET /v1/tdr/{year}/{storm_name}` | Every mission for that storm: `{mission_id, aircraft, tail_num, has_level1b, has_level2}[]`. |
| `GET /v1/tdr/mission/{mission_id}` | One mission's full product index: `{..., file_count, files: [{level, product, format, analysis_time, storm_relative, fall_speed_removed, source_url}]}`. `product` is one of `xy`, `xy_rel`, `vert_inbound`, `vert_inbound_rel`, `vert_inbound_fall`, `vert_outbound`, `vert_outbound_rel`, `vert_outbound_fall`, `awips_maxdb`, `awips_wind` — the plain/`_rel`/`_fall` variants of a vertical profile are genuinely separate files at the same analysis time, not the same file with a flag. `source_url` points directly at the original NOAA host (not proxied through this API yet). |

```bash
curl "https://joshmurdock.net/api/v1/tdr/2024/Beryl"
# {"year":2024,"storm_name":"Beryl","missions":[
#   {"mission_id":"20240630I1","aircraft":"NOAA 43 (Miss Piggy)","tail_num":"N43",
#    "has_level1b":true,"has_level2":true}, ...]}
```

## `GET /v1/tdr/sweep` 🟢

Fetches one indexed product file (lazily, cached under `cache/tdr_nc/` on
first request — see `services/tdr_nc.rs`), decodes it, and slices out a
single 2D plane: a horizontal CAPPI for an `xy`/`xy_rel` volume, or the
whole `(radius, height)` grid for a `vert_inbound`/`vert_outbound` profile
(those files are already 2D — no level selection needed). Response shape
mirrors the hurricanes site's `js/tdr-archive.js` (originally built against
a third-party API, TC-Atlas): a storm-relative grid (`x`/`y` in km), a
`data` 2D array, and a Plotly-style `colorscale` — the same client
rendering code works against either source.

**Verified against the post-2021 Cartesian grid schema only** (`x`/`y`
dims + 2D `LATITUDE`/`LONGITUDE`, per the AOML TDR README's 2021
gridding-change note). A pre-2021 file (regularly-spaced lat/lon grid,
`lats`/`lons` 1D variables) returns `400` rather than guessing at that
schema's layout — nobody has inspected a real one yet.

### Query parameters

| Param | Type | Default | Notes |
|---|---|---|---|
| `mission_id` | string | *required* | e.g. `20240630I1`. |
| `product` | string | *required* | `xy`, `xy_rel`, `vert_inbound`, `vert_inbound_rel`, `vert_inbound_fall`, `vert_outbound`, `vert_outbound_rel`, `vert_outbound_fall` — see `GET /v1/tdr/mission/{id}` for what a given mission actually has. |
| `analysis_time` | string | *required* | `HHMM`, matching one of the mission's indexed analysis times. |
| `field` | string | *required* | `xy`/`xy_rel`: `reflectivity`, `radial_wind`, `tangential_wind`, `u`, `v`, `w`, `vort`, `wind_speed`. `vert_*`: `reflectivity`, `radial_wind`, `tangential_wind`, `wind_speed`. |
| `level` | string | resolved | `1b` or `2` (which source level's file to read). Defaults to `2` if that mission has a Level 2 file, else `1b`. |
| `z` | float | `2.0` | `xy`/`xy_rel` only — CAPPI altitude in km, snapped to the nearest actual analysis level (echoed back as `z_km`). Ignored for `vert_*` (no level axis). |

```bash
curl "https://joshmurdock.net/api/v1/tdr/sweep?mission_id=20240630I1&product=xy&analysis_time=1201&field=reflectivity&z=2.0"
```

```json
{
  "mission_id": "20240630I1",
  "storm_name": "BERYL",
  "level": "2",
  "product": "xy",
  "analysis_time": "1201",
  "field": "reflectivity",
  "z_km": 2.0,
  "x": [-249.0, -247.0, "...", 249.0],
  "y": [-249.0, -247.0, "...", 249.0],
  "data": [["...250x250 values, null where no data..."]],
  "colorscale": [[0.0, "#04e9e7"], ["...", "..."], [1.0, "#ffffff"]],
  "zmin": 0.0,
  "zmax": 70.0,
  "units": "dBZ",
  "origin_lat": 10.53,
  "origin_lon": -53.96
}
```

- `x`/`y` are km from the grid origin (`origin_lat`/`origin_lon`, the storm
  center at analysis time) for an `xy` product, or along-track radius (`x`)
  and height (`y`) in km for a `vert_*` product — `z_km`/`origin_lat`/
  `origin_lon` are `null` in that case, since a vertical profile has no
  level axis or fixed grid origin.
- `data[row][col]` — for `xy`, row indexes `y` and col indexes `x` (a
  north-up map); for `vert_*`, row indexes `y`/height and col indexes
  `x`/radius (a bottom-up cross-section). A cell is `null` wherever the
  source file's `missing_value` sentinel (commonly `-999.9`, not
  `_FillValue`) appears — no radar coverage at that point.
- `colorscale`/`zmin`/`zmax` are a **suggested** default (reflectivity gets
  the common green→yellow→red→magenta radar convention; every wind field
  except `wind_speed` gets a blue-white-red diverging scale since sign is
  physically meaningful — inbound vs outbound, updraft vs downdraft).
  Nothing stops a client from ignoring them and supplying its own.

### Error responses

- `404` — unknown `mission_id`, or no file on record for the given
  `level`/`product`/`analysis_time` (check `GET /v1/tdr/mission/{id}` for
  what's actually indexed).
- `400` — unknown `field` for the given `product`, or a pre-2021
  lat/lon-gridded file (unsupported schema — see above).
- `502` — the upstream NOAA host couldn't be reached to fetch the source file.

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
into the cache. Also a "Databases" panel showing storm-track and recon MET
archive size/record counts, a browsable viewer (year -> storm -> track
points/missions) over both, and a "force update" button per archive to
run the nightly ingest on demand instead of waiting for the timer. Static
page at `app/console/index.html`, calling the `/v1/admin/*` JSON endpoints
below.

Every console user has their own account (username + password) with one
of two roles — see "API management" below for how accounts/tokens are
created:

- **superuser** — everything, including the API management pane (tokens,
  login log, the public-auth toggle) and triggering self-update.
- **moderator** — everything *except* API management and self-update
  (cache, logs, databases, archive-rebuild triggers, usage log).

The very first account is bootstrapped automatically from the legacy
single-shared-admin credentials file (`admin_credentials.json`, gitignored,
default `admin`/`password` — change it before exposing this publicly) the
first time the app starts after upgrading to token auth; see the README's
"Admin console" section.

```
https://joshmurdock.net/api/
```

### `/v1/admin/*` endpoints (all require a logged-in session except login/whoami/public-stats)

| Endpoint | Method | Purpose |
|---|---|---|
| `/v1/admin/public-stats` | GET | **No login required.** Shown on the console's login screen so anyone can see basic health before authenticating: `{healthy, uptime_seconds, calls_last_hour, total_calls}`. Deliberately excludes cache/storage figures — those stay behind login in `/status` below. In-memory counters, reset on process restart. |
| `/v1/admin/login` | POST | `{username, password}` JSON body → sets session cookie and returns `{status, role, username}`, or `401`. Every attempt (success or failure) is recorded to the login log. |
| `/v1/admin/logout` | POST | Clears the session. |
| `/v1/admin/whoami` | GET | `{authenticated: bool, role, username}` — no login required, used by the console to decide whether to show the login form and which sections a given role should see. |
| `/v1/admin/status` | GET | Cache stats (`satellite`/`goes_nc` file count + bytes + total) **and** database stats: `databases.storms` (`bytes`, `storm_count`), `databases.recon_met` (`bytes`, `mission_count`), a `databases.total_bytes`, and a `grand_total_bytes` across everything. |
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
| `/v1/admin/archive-update/{archive}` | POST | Force-run the storms or recon MET nightly ingest immediately — `archive` is `storms` or `recon_met`. Same code path as the systemd timer (`storms.run_ingest()` / `recon_met.run_ingest()`), just triggered on demand for data that hasn't been picked up yet. `409` if that archive's update is already running (singleton per archive, not job-id-keyed like `/prefetch`). |
| `/v1/admin/archive-update/{archive}` | GET | Poll that archive's update status: `{status: idle\|queued\|running\|done, started_at, finished_at, summary, error}`. |
| `/v1/admin/self-update/status` | GET | Cached "is an update available" check plus any in-progress apply job. |
| `/v1/admin/self-update/check` | POST | **Superuser only.** Force an immediate GitHub check, bypassing the periodic timer. |
| `/v1/admin/self-update/apply` | POST | **Superuser only.** Pull the latest code and restart the process. |
| `/v1/admin/self-update/apply` | GET | Poll the in-progress apply job (same as `/self-update/status`'s `job` field). |

### API management (superuser only, except usage log)

| Endpoint | Method | Purpose |
|---|---|---|
| `/v1/admin/tokens` | GET | List every token/account (role, owner, username, timestamps, revoked) — never the raw secret or password. |
| `/v1/admin/tokens` | POST | Create a token. `{role, owner_name, owner_email?, notes?, username?, password?}` — `username`/`password` required unless `role` is `regular`. Returns the raw token **once**; it can't be retrieved again (only regenerated, invalidating the old one). |
| `/v1/admin/tokens/{id}` | PATCH | Edit `owner_name`/`owner_email`/`notes`/`revoked`/`username`/`password`. |
| `/v1/admin/tokens/{id}` | DELETE | Permanently delete. Usage/login log entries keep their own snapshot of owner/role/username, so history isn't lost. |
| `/v1/admin/tokens/{id}/regenerate` | POST | Issue a new secret for an existing token, invalidating the old one. Same one-time-reveal behavior as create. |
| `/v1/admin/login-log` | GET | `?limit=` (default 200, max 1000) most recent console login attempts — username, role, success/failure, IP, user agent, timestamp. Superuser-only since it reveals other admins' usernames/IPs. |
| `/v1/admin/usage-log` | GET | `?token_id=&limit=` (default 200, max 1000) most recent public-API calls — owner, role, endpoint, method, status code, IP, timestamp. Visible to moderators too (it's usage data, not a permissions surface). Empty unless auth is enabled and at least one token has been used. |
| `/v1/admin/auth-config` | GET | `{enabled: bool}` — whether the public API currently requires a token. |
| `/v1/admin/auth-config` | POST | `{enabled: bool}` — flip it. Takes effect immediately, no restart. |

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
