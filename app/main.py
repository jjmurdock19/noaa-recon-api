from fastapi import FastAPI
from fastapi.middleware.cors import CORSMiddleware
from fastapi.responses import PlainTextResponse
from fastapi.staticfiles import StaticFiles
from starlette.middleware.sessions import SessionMiddleware

from app import auth
from app.paths import CACHE_ROOT, REPO_ROOT
from app.routers import admin, health, raw, satellite, tdr

app = FastAPI(
    title="noaa-recon-api",
    description=(
        "Open-source API for archival GOES satellite imagery (Band 13 IR, "
        "Band 2 visible, GeoColor) and NOAA Tail Doppler Radar (TDR) data, "
        "with a raw-netCDF passthrough for client-side rendering."
    ),
    version="0.1.0",
)

# Open CORS: this API is explicitly meant to be consumed by other websites,
# not just the hurricanes site that proxies it same-origin via nginx. This
# only governs cross-origin requests — the admin console (which needs the
# session cookie below) is always used same-origin, so CORS doesn't apply
# to it regardless of allow_methods here.
app.add_middleware(
    CORSMiddleware,
    allow_origins=["*"],
    allow_methods=["GET"],
    allow_headers=["*"],
)

# Signed-cookie session for the admin console (app/auth.py, app/routers/admin.py).
# Secret key is generated once into the gitignored admin_credentials.json on
# first run, not hardcoded — see app/auth.py.
app.add_middleware(SessionMiddleware, secret_key=auth.get_secret_key())

app.mount("/cache", StaticFiles(directory=str(CACHE_ROOT)), name="cache")
app.mount(
    "/demo/netcdf-three",
    StaticFiles(directory=str(REPO_ROOT / "clients" / "netcdf-three-demo"), html=True),
    name="netcdf-three-demo",
)

app.include_router(health.router, prefix="/v1")
app.include_router(satellite.router, prefix="/v1")
app.include_router(tdr.router, prefix="/v1")
app.include_router(raw.router, prefix="/v1")
app.include_router(admin.router, prefix="/v1")


@app.get("/llms.txt", response_class=PlainTextResponse, tags=["docs"])
async def llms_txt():
    """Agent-discovery summary per the llms.txt convention (https://llmstxt.org/)."""
    return (REPO_ROOT / "llms.txt").read_text()


# Admin console UI — mounted LAST (after /v1/*, /cache, /demo/*, /llms.txt)
# so those specific routes are matched first; this static mount at "/" is
# the catch-all, serving console/index.html for "/" and any other
# unmatched path under it. The page itself is static; it calls the /v1/admin/*
# JSON endpoints above (which enforce login) for everything dynamic.
app.mount("/", StaticFiles(directory=str(REPO_ROOT / "app" / "console"), html=True), name="console")
