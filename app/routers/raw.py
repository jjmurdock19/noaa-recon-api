"""Raw netCDF passthrough — STUB, follow-up phase.

Planned shape: `GET /v1/raw/netcdf?data_type=satellite|tdr&band_or_variable=...
&time=...&center=lat,lon&dims=km_x,km_y` subsets the source netCDF (via
netCDF4 variable slicing, not just proxying the full S3/archive file) to the
requested center/dimensions and streams it back with
`Content-Type: application/x-netcdf`. This is the feed for the
`clients/netcdf-three-demo/` page and the hurricanes site's "Raw Data Viewer
(3D)" panel (see `js/goes-tdr-3d.js` in the hurricanes repo).

For the GOES MVP slice this can subset directly from the same downloaded
ABI-L2-CMIPF file `app/services/goes.py` already fetches (no new data
source needed) — implement once the satellite tile path is validated. The
TDR side depends on the TDR crawler/parsing work in `app/services/tdr.py`.
"""
from fastapi import APIRouter, HTTPException

router = APIRouter(prefix="/raw", tags=["raw"])


@router.get("/netcdf")
async def get_raw_netcdf():
    raise HTTPException(
        501,
        "Not implemented yet. Raw netCDF passthrough is a follow-up phase — see "
        "app/routers/raw.py for the planned request/response shape.",
    )
