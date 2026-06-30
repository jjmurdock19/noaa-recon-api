"""Tail Doppler Radar endpoints.

STUB — follow-up phase. Real implementation requires a crawler/index over
https://seb.omao.noaa.gov/pub/acdata/ (year -> `YYYYMMDD[N|I|H]#/` mission
directories -> per-instrument `.tar.gz` bundles; no manifest exists, so a
local index has to be built and refreshed by a background crawl job) plus
extraction and netCDF parsing of the raw TDR sweeps. See the repo README's
"Agentic instructions" section for the planned shape of these endpoints.
"""
from fastapi import APIRouter, HTTPException

router = APIRouter(prefix="/tdr", tags=["tdr"])


@router.get("/missions")
async def list_missions():
    raise HTTPException(
        501,
        "Not implemented yet. TDR mission indexing (crawling seb.omao.noaa.gov, "
        "extracting .tar.gz bundles, parsing raw netCDF) is a follow-up phase.",
    )


@router.get("/sweep")
async def get_sweep():
    raise HTTPException(
        501,
        "Not implemented yet. TDR sweep rendering is a follow-up phase — see "
        "app/services/tdr.py and the README's TDR section.",
    )
