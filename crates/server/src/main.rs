//! Application entry point — port of `app/main.py`.
//!
//! Builds the axum app (the FastAPI equivalent), wires middleware and routers,
//! and serves. Ported incrementally: right now only `/v1/health` and the
//! `/llms.txt` doc route exist; static mounts (`/cache`, `/demo`, console) and
//! the per-request logging + token-usage middleware come as their modules land.

mod auth;
mod config;
mod error;
mod logging;
mod routers;
mod services;
mod state;

// Shared, WASM-safe types (models::TileStatus, …) now live in the core crate;
// modules import them directly via `noaa_recon_core::models::…` as they need them.

use std::time::Duration;

use axum::{
    extract::{Request, State},
    http::header,
    middleware::{self, Next},
    response::Response,
    routing::get,
    Router,
};
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::ServeDir;

use crate::config::Paths;
use crate::state::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // paths.py: resolve repo root, ensure cache/ and data/ exist.
    let paths = Paths::resolve()?;

    // logging_config.py: rotating file + stdout. Keep the guard alive for the
    // whole process (dropping it stops the async file writer).
    let _log_guard = logging::configure(&paths.repo_root)?;

    // Must run before any netCDF file is opened — see hdf5_zstd.rs. Every
    // entry point below (serve, and both ingest/cache subcommands) can open
    // a GOES netCDF file, so this runs unconditionally ahead of the
    // subcommand dispatch.
    services::hdf5_zstd::register();

    // Subcommands (replace the Python maintenance scripts). No subcommand => serve.
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("ingest-storms") => return cmd_ingest_storms(&paths).await,
        Some("ingest-recon") => return cmd_ingest_recon(&paths, &args).await,
        Some("clean-nc-cache") => return cmd_clean_nc_cache(&paths, &args),
        Some("--help" | "-h") => {
            eprintln!("usage: noaa-recon-api [ingest-storms | ingest-recon [--years Y,Y] [--force] | clean-nc-cache [--max-age-hours N]]\n  (no subcommand: run the HTTP server)");
            return Ok(());
        }
        Some(other) => anyhow::bail!("unknown subcommand '{other}' (try --help)"),
        None => {}
    }

    // Capture the paths the static mounts need before `paths` is moved into state.
    let llms_txt_path = paths.llms_txt();
    let cache_dir = paths.cache_root.clone();
    let demo_dir = paths.netcdf_three_demo_dir();
    let console_dir = paths.console_dir();

    // Signed-cookie session key, derived from admin_credentials.json (auth.py).
    let creds = auth::load_credentials(&paths.repo_root)?;
    let cookie_key = auth::derive_cookie_key(&creds.secret_key);
    let state = AppState::new(paths, cookie_key);

    // One-time: seed the first superuser from admin_credentials.json if the
    // tokens table is empty (main.py's _migrate_legacy_admin startup hook).
    if let Ok(conn) = services::tokens::get_connection(&state.paths.auth_db) {
        if let Ok(true) = services::tokens::migrate_legacy_admin_credentials(
            &conn,
            &creds.username,
            &creds.password,
        ) {
            tracing::info!("Seeded first superuser '{}' from admin_credentials.json", creds.username);
        }
    }

    // Background: periodically refresh the cached self-update check so the
    // console can show an "update available" badge without an operator
    // click. Never pulls/restarts by itself — see main.py's
    // `_start_self_update_checker` (this mirrors it 1:1).
    spawn_self_update_checker(state.clone());

    // CORS: open, GET-only — mirrors main.py's CORSMiddleware(allow_origins=["*"],
    // allow_methods=["GET"]). This API is meant to be consumed cross-origin.
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([axum::http::Method::GET])
        .allow_headers(Any);

    // FastAPI: app.include_router(..., prefix="/v1"). health stays open; the
    // data routers (storms/tdr/raw/…) are gated by require_api_token in Python,
    // but that gate is a no-op unless auth is explicitly enabled (off by
    // default), so mounting them ungated here matches the default deployment.
    // The gate is added with auth.rs.
    // Data routers gated by require_api_token (no-op unless auth_config enables
    // it). health stays open; admin (own session auth) is added later.
    let gated = Router::new()
        .merge(routers::storms::router())
        .merge(routers::recon::router())
        .merge(routers::satellite::router())
        .merge(routers::tdr::router())
        .merge(routers::raw::router())
        .layer(middleware::from_fn_with_state(state.clone(), auth::require_api_token));
    // admin routers have their own session auth (not the token gate).
    let v1 = Router::new()
        .merge(routers::health::router())
        .merge(routers::admin::router())
        .merge(routers::admin_tokens::router())
        .merge(gated);

    // Static mounts — mirror main.py's app.mount(...) calls.
    //   /cache            -> StaticFiles(CACHE_ROOT)
    //   /demo/netcdf-three-> StaticFiles(..., html=True)
    // `html=True` == serve index.html for directory requests (append_index_html).
    let cache_service = ServeDir::new(&cache_dir);
    let demo_service = ServeDir::new(&demo_dir).append_index_html_on_directories(true);

    // The console static mount is FastAPI's LAST mount at "/", i.e. the catch-all
    // that serves console/index.html. In axum that's the router's fallback_service
    // so it only runs after every specific route (/v1/*, /cache, /demo, /llms.txt)
    // has had its chance to match.
    let console_service = ServeDir::new(&console_dir).append_index_html_on_directories(true);

    let app = Router::new()
        .nest("/v1", v1)
        .route("/llms.txt", get(move || llms_txt(llms_txt_path.clone())))
        .nest_service("/cache", cache_service)
        .nest_service("/demo/netcdf-three", demo_service)
        .fallback_service(console_service)
        // State-carrying middleware so it can bump the stats counter (main.py's
        // log_requests also called stats.record_request()).
        .layer(middleware::from_fn_with_state(state.clone(), log_requests))
        .layer(cors)
        .with_state(state);

    let addr = config::bind_addr();
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("noaa-recon-api (rust) listening on http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

/// `ingest-storms` subcommand — port of scripts/ingest_storms.py (HURDAT2 + ATCF).
async fn cmd_ingest_storms(paths: &Paths) -> anyhow::Result<()> {
    println!("Ingesting storm-track archive (HURDAT2 + ATCF) — this usually takes ~10s...");
    let summary = services::storms::run_ingest(&paths.storms_db).await?;
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}

/// `ingest-recon` subcommand — port of scripts/ingest_recon_met.py.
async fn cmd_ingest_recon(paths: &Paths, args: &[String]) -> anyhow::Result<()> {
    use chrono::Datelike;
    let mut years: Option<Vec<i64>> = None;
    let mut force = false;
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--years" => {
                years = args.get(i + 1).map(|s| {
                    s.split(',').filter_map(|y| y.trim().parse::<i64>().ok()).collect()
                });
                i += 2;
            }
            // --full: every season since the archive's first year (2011).
            "--full" => {
                let now = chrono::Utc::now().year() as i64;
                years = Some((2011..=now).collect());
                i += 1;
            }
            "--force" => {
                force = true;
                i += 1;
            }
            _ => i += 1,
        }
    }
    println!("Ingesting recon MET archive (crawl + netCDF + reconcile)...");
    let summary =
        services::recon_ingest::run_ingest(&paths.recon_met_db, &paths.storms_db, years, force).await?;
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}

/// `clean-nc-cache` subcommand — port of scripts/clear_nc_cache.py.
fn cmd_clean_nc_cache(paths: &Paths, args: &[String]) -> anyhow::Result<()> {
    let mut max_age_hours = 24.0_f64;
    let mut i = 2;
    while i < args.len() {
        if args[i] == "--max-age-hours" {
            max_age_hours = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(24.0);
            i += 2;
        } else {
            i += 1;
        }
    }
    let nc_dir = paths.cache_root.join("goes_nc");
    let (removed, freed) = services::goes::clean_nc_cache(&nc_dir, max_age_hours);
    println!("Freed {freed} bytes across {removed} file(s) older than {max_age_hours}h.");
    Ok(())
}

/// One log line per request + stats counter — port of main.py's `log_requests`
/// HTTP middleware. Per-token usage recording hooks in with auth.rs.
async fn log_requests(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let start = std::time::Instant::now();

    let response = next.run(request).await;

    let elapsed: Duration = start.elapsed();
    tracing::info!(
        "{} {} -> {} ({:.1}ms)",
        method,
        path,
        response.status().as_u16(),
        elapsed.as_secs_f64() * 1000.0,
    );
    state.stats.record_request();
    response
}

/// How often the background task below checks the git remote for new
/// commits. Only ever updates the cached "is an update available" status the
/// console reads (`/v1/admin/self-update/status`) — it never pulls or
/// restarts by itself; that's exclusively triggered by an operator hitting
/// "Update now" in the console (see services/self_update.rs).
const SELF_UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(1800);

fn spawn_self_update_checker(state: AppState) {
    tokio::spawn(async move {
        loop {
            match services::self_update::check_for_update(&state.paths.repo_root).await {
                Ok(result) => state.self_update.set_cached_check(Some(result), None),
                Err(e) => state.self_update.set_cached_check(None, Some(e)),
            }
            tokio::time::sleep(SELF_UPDATE_CHECK_INTERVAL).await;
        }
    });
}

/// Port of main.py's `/llms.txt` PlainTextResponse route.
async fn llms_txt(path: std::path::PathBuf) -> Response {
    match tokio::fs::read_to_string(&path).await {
        Ok(body) => (
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            body,
        )
            .into_response(),
        Err(_) => (axum::http::StatusCode::NOT_FOUND, "llms.txt not found").into_response(),
    }
}

use axum::response::IntoResponse;
