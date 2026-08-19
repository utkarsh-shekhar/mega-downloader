//! Headless engine server: a standalone binary exposing the engine over REST +
//! WebSocket on localhost. Running independently of the UI is the core
//! reliability decision — downloads survive a UI crash/reload. Phase 6 wraps
//! this same binary as a Tauri sidecar.

mod routes;
mod static_files;
mod ws;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::http::HeaderValue;
use axum::routing::{get, post};
use axum::Router;
use engine::EngineEvent;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};
use tower_http::trace::TraceLayer;

/// Shared application state handed to every request handler.
#[derive(Clone)]
pub struct AppState {
    pub pool: sqlx::SqlitePool,
    pub events: broadcast::Sender<EngineEvent>,
    /// Cancellation tokens for currently-running jobs (used to pause them).
    pub jobs: Arc<Mutex<HashMap<String, CancellationToken>>>,
}

/// Destination directory for downloads: `$DOWNLOAD_DIR` or `<cwd>/downloads`.
pub fn download_dir() -> PathBuf {
    std::env::var_os("DOWNLOAD_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap_or_default()
                .join("downloads")
        })
}

/// Address the engine listens on. `$BIND_ADDR`, defaulting to `127.0.0.1:8787`
/// (loopback) so a local process isn't reachable from the LAN. Container /
/// remote deployments should set `BIND_ADDR=0.0.0.0:8787`.
pub fn bind_addr() -> SocketAddr {
    std::env::var("BIND_ADDR")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], 8787)))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,server=debug,engine=debug".into()),
        )
        .init();

    // A filesystem path (not a URL): the Tauri sidecar points this at the OS
    // app-data dir; in dev it defaults to the working directory.
    let db_path = std::env::var_os("DB_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("mega-downloader.db"));
    let pool = engine::db::connect(&db_path).await?;
    tracing::info!("database ready at {}", db_path.display());

    let dest = download_dir();
    tokio::fs::create_dir_all(&dest).await.ok();
    tracing::info!("download directory: {}", dest.display());

    let (events, _) = broadcast::channel::<EngineEvent>(1024);
    let state = AppState {
        pool,
        events,
        jobs: Arc::new(Mutex::new(HashMap::new())),
    };

    // Resume any unfinished jobs from a previous run.
    resume_unfinished_jobs(state.clone());

    let app = Router::new()
        .route("/api/health", get(routes::health))
        .route("/api/inspect", post(routes::inspect))
        .route(
            "/api/settings",
            get(routes::get_settings).post(routes::save_settings),
        )
        .route("/api/jobs", get(routes::list_jobs).post(routes::create_job))
        .route("/api/jobs/{id}", get(routes::get_job).delete(routes::delete_job))
        .route("/api/jobs/{id}/retry", post(routes::retry_job))
        .route("/api/jobs/{id}/pause", post(routes::pause_job))
        .route("/api/jobs/{id}/resume", post(routes::resume_job))
        .route("/api/jobs/{id}/status", get(routes::job_status))
        .route("/api/jobs/{id}/zip", get(routes::job_zip))
        .route("/ws", get(ws::handler))
        // Only our own UIs may call the engine cross-origin: the Vite dev
        // server and the packaged Tauri webview. A permissive CORS here would
        // let any webpage in the user's browser drive the downloader
        // (change the download dir, queue arbitrary content, delete jobs).
        .layer(
            CorsLayer::new()
                .allow_origin(AllowOrigin::list([
                    HeaderValue::from_static("http://localhost:5173"),
                    HeaderValue::from_static("http://127.0.0.1:5173"),
                    HeaderValue::from_static("tauri://localhost"),
                    HeaderValue::from_static("http://tauri.localhost"),
                    HeaderValue::from_static("https://tauri.localhost"),
                ]))
                .allow_methods(Any)
                .allow_headers(Any),
        )
        .layer(TraceLayer::new_for_http())
        // Self-contained single-process mode: serve the compiled UI from this
        // same process (mounted after the API/WS routes so those always win).
        .fallback_service(static_files::fallback(&static_files::ui_dir()))
        .with_state(state);

    let addr = bind_addr();
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("engine listening on http://{addr}");
    axum::serve(listener, app).await?;

    Ok(())
}

/// On startup, continue downloading any jobs left actively running. Jobs the
/// user explicitly paused stay paused, and jobs that ended in error wait for
/// an explicit retry (auto-retrying them every launch would hammer files that
/// failed permanently).
fn resume_unfinished_jobs(state: AppState) {
    tokio::spawn(async move {
        let Ok(Some(token)) = routes::get_setting(&state.pool, "rd_token").await else {
            return; // no token configured yet — nothing to resume with
        };

        let jobs: Vec<(String,)> = sqlx::query_as(
            "SELECT id FROM jobs WHERE status IN ('downloading', 'pending') ORDER BY created_at",
        )
        .fetch_all(&state.pool)
        .await
        .unwrap_or_default();

        for (job_id,) in jobs {
            tracing::info!("resuming job {job_id}");
            let dest = routes::job_dest(&state.pool, &job_id).await;
            let downloader = routes::build_downloader(&state, token.clone(), dest).await;
            routes::spawn_job(state.clone(), downloader, job_id);
        }
    });
}
