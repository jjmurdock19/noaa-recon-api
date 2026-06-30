"""Tail Doppler Radar data service — STUB, follow-up phase.

Planned shape (see plan / README "Agentic instructions"):

  1. Mission crawler: walk https://seb.omao.noaa.gov/pub/acdata/{year}/ to
     discover `YYYYMMDD[N|I|H]#/` mission directories (N=normal,
     I=interim, H=hurricane), and the `RADAR_TDR/` subdirectory's
     `.tar.gz` bundles within each. There is no manifest/index published
     by NOAA, so this has to build and persist its own local index
     (e.g. a small SQLite table: mission_id, date, storm, tar_gz_url(s)).

  2. Extraction + parsing: download a mission's `.tar.gz`, extract the raw
     TDR netCDF sweep files, and parse them (variables, dims, and
     elevation/level structure will need to be inspected from a real
     sample file — not yet done as of this stub).

  3. Rendering: turn a parsed sweep into the same storm-relative
     200x200 km grid + Plotly-style colorscale response shape already
     consumed by the hurricanes site's `tdr-archive.js` (see that file's
     `_fetch('data', ...)` response handling for the exact shape to match),
     so the existing client-side renderer needs minimal changes when it's
     later migrated onto this API.
"""
