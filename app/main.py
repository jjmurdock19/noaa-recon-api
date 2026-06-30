from fastapi import FastAPI
from fastapi.middleware.cors import CORSMiddleware
from fastapi.staticfiles import StaticFiles

from app.paths import CACHE_ROOT, REPO_ROOT
from app.routers import health, raw, satellite, tdr

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
# not just the hurricanes site that proxies it same-origin via nginx.
app.add_middleware(
    CORSMiddleware,
    allow_origins=["*"],
    allow_methods=["GET"],
    allow_headers=["*"],
)

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
