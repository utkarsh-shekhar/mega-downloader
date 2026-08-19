//! Self-contained static UI serving.
//!
//! In desktop/Tauri mode the React UI talks to this engine over an absolute
//! localhost URL, so the engine only exposes the REST + WebSocket API. For the
//! Linux/container build we want ONE binary serving BOTH the API and the
//! compiled frontend, so a user (or a Docker/K8s image) can run a single
//! self-contained process.
//!
//! This module serves the built UI (a Vite `dist` folder) so that:
//!
//! - `/api/*` and `/ws` keep routing to the engine handlers (they are mounted
//!   on the main router, which always precedes this fallback service),
//! - every other request serves a static file from `$MEGA_UI_DIR`
//!   (default: `./ui-dist`),
//! - non-file paths (and the bare `/`) fall back to `index.html`, so a
//!   client-side-routed single page app still works.

use std::path::{Path, PathBuf};

use axum::Router;
use tower_http::services::{ServeDir, ServeFile};

/// Directory that holds the compiled frontend. `$MEGA_UI_DIR`, else `<cwd>/ui-dist`.
pub fn ui_dir() -> PathBuf {
    std::env::var_os("MEGA_UI_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap_or_default()
                .join("ui-dist")
        })
}

/// Build the router whose fallback service serves the compiled UI. Mounted as
/// the app's `fallback_service`: `/api` and `/ws` routes registered on the main
/// router take precedence over everything here.
pub fn fallback(dist: &Path) -> Router<()> {
    let dist = dist.to_path_buf();

    // Serve files from the dist dir. Directories serve their index.html;
    // any path that isn't a real file (an SPA route) serves index.html with a
    // normal 200. `fallback` (not `not_found_service`) is what gives the 200.
    let serve = ServeDir::new(&dist)
        .append_index_html_on_directories(true)
        .fallback(ServeFile::new(dist.join("index.html")));

    Router::new().fallback_service(serve)
}
