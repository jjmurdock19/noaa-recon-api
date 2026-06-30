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
| `GET /v1/satellite/tile` (Band 2 / GeoColor) | 🟡 Planned |
| `GET /v1/tdr/missions` | 🟡 Planned |
| `GET /v1/tdr/sweep` | 🟡 Planned |
| `GET /v1/raw/netcdf` | 🟡 Planned |
| `GET /demo/netcdf-three/` (static 3D client) | 🟢 Live (sample data only until raw passthrough ships) |

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
| `cmap` | string | `bd` | One of `bd`, `enhanced`, `nrl`, `grayscale` — see color tables below. |
| `satellite` | string | `goes-east` | Only `goes-east` is implemented (auto-resolves GOES-16 vs GOES-19 by date). `goes-west` returns `400`. |

### Color tables (`cmap`)

| Value | Description |
|---|---|
| `bd` | Standard NWS BD enhancement — greyscale for warm/moderate tops, blue→purple→red for cold convection |
| `enhanced` | Darker surface/low clouds, white mid/high clouds, color for coldest tops |
| `nrl` | Naval Research Lab tropical cyclone enhancement — smooth yellow-green→cyan→blue→purple→red ramp |
| `grayscale` | Plain linear greyscale by brightness temperature |

### Example request

```bash
curl "https://joshmurdock.net/api/v1/satellite/tile?time=2024-09-28T12:00:00Z&band=13&cmap=bd"
```

First call (job started):
```json
{"status": "generating", "key": "goes_13_bd_16_20240928T115621"}
```

After polling to completion:
```json
{
  "status": "ready",
  "key": "goes_13_bd_16_20240928T115621",
  "png_url": "/cache/satellite/goes_13_bd_16_20240928T115621.png",
  "bounds": [[-81.3, -156.0], [81.3, 6.0]],
  "band": 13,
  "cmap": "bd",
  "satellite": "GOES-16",
  "sat_lon": -75.0,
  "scan_start": "2024-09-28T11:56:21+00:00"
}
```

- `png_url` is **relative to the API's own base URL**, not the page you're
  integrating into — prefix it yourself: `base + png_url`. It is a
  2048×2048 RGBA PNG; transparent pixels are off-disk/no-data.
- `bounds` is `[[lat_south, lon_west], [lat_north, lon_east]]` — the exact
  format `L.imageOverlay(url, bounds)` expects in Leaflet.
- `scan_start` is the **actual** scan time used, which may differ from your
  requested `time` by a few minutes (nearest-match).

### Error response

```json
{"status": "error", "key": "...", "message": "No GOES Band 13 scan found near ..."}
```

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

async function loadGoesTile(map, { time, band = 13, cmap = 'bd' }) {
  const params = new URLSearchParams({ time, band, cmap });
  let data = await fetch(`${API_BASE}/v1/satellite/tile?${params}`).then(r => r.json());

  while (data.status === 'generating') {
    await new Promise(res => setTimeout(res, 3000));
    data = await fetch(`${API_BASE}/v1/satellite/status/${data.key}`).then(r => r.json());
  }
  if (data.status !== 'ready') throw new Error(data.message || 'tile render failed');

  return L.imageOverlay(API_BASE + data.png_url, data.bounds, { opacity: 0.85 }).addTo(map);
}

// loadGoesTile(map, { time: new Date().toISOString() });
```

(This is exactly the pattern used in the hurricanes tracker site's `js/api-explorer.js` — see that file for the full version with status-polling UI, band/colormap pickers, etc.)

### Python — fetch and save the PNG locally

```python
import time, requests

API_BASE = "https://joshmurdock.net/api"

def fetch_goes_tile(time_iso, band=13, cmap="bd"):
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
  `(band, cmap, satellite, resolved-scan-time)` — repeated identical
  requests are cheap (cache hit), not re-rendered.
- The OpenAPI schema at `{base}/openapi.json` is generated directly from
  the route definitions and is the most authoritative machine-readable
  source if this document drifts.
