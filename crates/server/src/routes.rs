//! REST handlers.

use std::path::PathBuf;

use axum::body::Body;
use axum::extract::{Path, Query};
use axum::http::{header, StatusCode};
use axum::response::Response;
use axum::{extract::State, Json};
use engine::{Downloader, RealDebrid};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::SqlitePool;
use tokio_util::io::ReaderStream;
use tokio_util::sync::CancellationToken;

use crate::{download_dir, AppState};

/// Liveness + version probe. Also confirms the DB pool is reachable.
pub async fn health(State(state): State<AppState>) -> Json<Value> {
    let db_ok = sqlx::query_scalar::<_, i64>("SELECT 1")
        .fetch_one(&state.pool)
        .await
        .is_ok();

    Json(json!({
        "status": "ok",
        "version": engine::VERSION,
        "db": db_ok,
    }))
}

#[derive(Deserialize)]
pub struct InspectReq {
    pub link: String,
}

/// The "structure brain" endpoint: given a MEGA folder link, return the fully
/// reconstructed (decrypted) node tree. No bytes are downloaded — this is the
/// free, unmetered listing step.
pub async fn inspect(Json(req): Json<InspectReq>) -> Result<Json<Value>, (StatusCode, String)> {
    let link = engine::mega::parse(&req.link).map_err(bad_request)?;
    let tree = engine::mega::fetch_tree(&link).await.map_err(bad_request)?;
    let body = serde_json::to_value(tree).map_err(internal)?;
    Ok(Json(body))
}

#[derive(Deserialize)]
pub struct SettingsReq {
    pub rd_token: Option<String>,
    pub download_dir: Option<String>,
    pub concurrency: Option<u32>,
    pub aria2_rpc_url: Option<String>,
    pub aria2_rpc_secret: Option<String>,
    pub max_download_speed: Option<String>,
}

/// Persist settings: Real-Debrid token, download directory, and concurrency.
/// A present-but-empty value clears that setting (back to the default); an
/// omitted field is left unchanged.
pub async fn save_settings(
    State(state): State<AppState>,
    Json(req): Json<SettingsReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    if let Some(token) = req.rd_token {
        upsert_or_clear(&state.pool, "rd_token", token.trim())
            .await
            .map_err(internal_sqlx)?;
    }
    if let Some(dir) = req.download_dir {
        upsert_or_clear(&state.pool, "download_dir", dir.trim())
            .await
            .map_err(internal_sqlx)?;
    }
    if let Some(c) = req.concurrency {
        set_setting(&state.pool, "concurrency", &c.clamp(1, 16).to_string())
            .await
            .map_err(internal_sqlx)?;
    }
    if let Some(u) = req.aria2_rpc_url {
        upsert_or_clear(&state.pool, "aria2_rpc_url", u.trim())
            .await
            .map_err(internal_sqlx)?;
    }
    if let Some(secret) = req.aria2_rpc_secret {
        upsert_or_clear(&state.pool, "aria2_rpc_secret", secret.trim())
            .await
            .map_err(internal_sqlx)?;
    }
    if let Some(speed) = req.max_download_speed {
        upsert_or_clear(&state.pool, "max_download_speed", speed.trim())
            .await
            .map_err(internal_sqlx)?;
    }
    Ok(Json(json!({ "ok": true })))
}

/// Report current settings (token presence only, never the token itself).
pub async fn get_settings(State(state): State<AppState>) -> Json<Value> {
    let token_set = get_setting(&state.pool, "rd_token")
        .await
        .ok()
        .flatten()
        .is_some();
    let aria2_url = get_setting(&state.pool, "aria2_rpc_url")
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    let aria2_secret_set = get_setting(&state.pool, "aria2_rpc_secret")
        .await
        .ok()
        .flatten()
        .is_some();
    let max_speed = get_setting(&state.pool, "max_download_speed")
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    Json(json!({
        "rd_token_set": token_set,
        "download_dir": resolve_dest(&state.pool).await.to_string_lossy(),
        "concurrency": resolve_concurrency(&state.pool).await,
        "aria2_rpc_url": aria2_url,
        "aria2_rpc_secret_set": aria2_secret_set,
        "max_download_speed": max_speed,
    }))
}

/// Effective download directory: the saved setting, else the env/default.
pub async fn resolve_dest(pool: &SqlitePool) -> std::path::PathBuf {
    match get_setting(pool, "download_dir").await.ok().flatten() {
        Some(d) if !d.trim().is_empty() => PathBuf::from(d),
        _ => download_dir(),
    }
}

/// Effective concurrency: the saved setting (1..=16), else the default.
pub async fn resolve_concurrency(pool: &SqlitePool) -> usize {
    get_setting(pool, "concurrency")
        .await
        .ok()
        .flatten()
        .and_then(|s| s.parse::<usize>().ok())
        .map(|c| c.clamp(1, 16))
        .unwrap_or(engine::download::DEFAULT_CONCURRENCY)
}

/// GET /api/path-mappings — list remote path mappings.
pub async fn list_path_mappings(State(state): State<AppState>) -> Json<Value> {
    Json(json!({ "mappings": path_mappings_all(&state.pool).await }))
}

#[derive(Deserialize)]
pub struct PathMappingReq {
    pub remote_path: String,
    pub local_path: String,
}

/// POST /api/path-mappings — add a mapping.
pub async fn add_path_mapping(
    State(state): State<AppState>,
    Json(req): Json<PathMappingReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let remote = engine::pathmap::normalize(&req.remote_path);
    let local = engine::pathmap::normalize(&req.local_path);
    if remote.is_empty() || local.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "remote_path and local_path are required".into(),
        ));
    }
    let position: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM path_mappings")
        .fetch_one(&state.pool)
        .await
        .map_err(internal_sqlx)?;
    let id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO path_mappings (remote_path, local_path, position) VALUES (?, ?, ?) RETURNING id",
    )
    .bind(&remote)
    .bind(&local)
    .bind(position)
    .fetch_one(&state.pool)
    .await
    .map_err(internal_sqlx)?;
    Ok(Json(json!({ "ok": true, "id": id })))
}

/// DELETE /api/path-mappings/{id} — remove a mapping.
pub async fn delete_path_mapping(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let affected = sqlx::query("DELETE FROM path_mappings WHERE id = ?")
        .bind(id)
        .execute(&state.pool)
        .await
        .map_err(internal_sqlx)?
        .rows_affected();
    if affected == 0 {
        return Err((StatusCode::NOT_FOUND, format!("no mapping with id {id}")));
    }
    Ok(Json(json!({ "ok": true })))
}

/// GET /api/aria2/status — probe Aria2 connectivity + report speed limit.
pub async fn aria2_status(State(state): State<AppState>) -> Json<Value> {
    match aria2_config(&state.pool).await {
        Some(cfg) => {
            match engine::aria2::Aria2Client::from_config(&cfg) {
                Some(client) => match client.get_version().await {
                    Ok(v) => Json(json!({
                        "connected": true,
                        "version": v,
                        "max_download_speed": cfg.max_download_limit,
                    })),
                    Err(e) => Json(json!({
                        "connected": false,
                        "error": e.to_string(),
                        "max_download_speed": cfg.max_download_limit,
                    })),
                },
                None => Json(json!({ "connected": false, "configured": true, "error": "invalid Aria2 config" })),
            }
        }
        None => Json(json!({ "connected": false, "configured": false })),
    }
}

/// Build a Downloader writing to `dest` at the current concurrency setting.
pub async fn build_downloader(
    state: &AppState,
    token: String,
    dest: std::path::PathBuf,
) -> Downloader {
    let concurrency = resolve_concurrency(&state.pool).await;
    let mut dl = Downloader::new(
        state.pool.clone(),
        state.events.clone(),
        RealDebrid::new(token),
        dest,
        concurrency,
    );
    // Wire up the optional Aria2 backend + remote path mappings from settings.
    if let Some(aria2_cfg) = aria2_config(&state.pool).await {
        dl.with_aria2(&aria2_cfg);
    }
    let mappings = path_mappings_all(&state.pool).await;
    if !mappings.is_empty() {
        dl.set_path_mappings(mappings);
    }
    dl
}

/// The destination a job was created with, so resume/retry write to the same
/// place even if the default download dir setting later changes.
pub async fn job_dest(pool: &SqlitePool, job_id: &str) -> std::path::PathBuf {
    sqlx::query_scalar::<_, String>("SELECT root_path FROM jobs WHERE id = ?")
        .bind(job_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .map(PathBuf::from)
        .unwrap_or_else(download_dir)
}

#[derive(Deserialize)]
pub struct CreateJobReq {
    pub link: String,
    /// Optional subset of file handles to download (defaults to all files).
    #[serde(default)]
    pub include_handles: Option<Vec<String>>,
}

/// Create a download job from a link and start processing it in the background.
pub async fn create_job(
    State(state): State<AppState>,
    Json(req): Json<CreateJobReq>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let token = get_setting(&state.pool, "rd_token")
        .await
        .ok()
        .flatten()
        .ok_or((
            StatusCode::BAD_REQUEST,
            "Real-Debrid token not set — save it in settings first".to_string(),
        ))?;

    let dest = resolve_dest(&state.pool).await;
    let downloader = build_downloader(&state, token, dest).await;

    let (job_id, tree) = downloader
        .create_job(&req.link, req.include_handles)
        .await
        .map_err(bad_request)?;

    let body = json!({ "job_id": job_id, "tree": serde_json::to_value(tree).map_err(internal)? });
    spawn_job(state, downloader, job_id);
    Ok(Json(body))
}

/// Requeue a job's failed files and resume downloading.
pub async fn retry_job(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    // A running job must not be retried: resetting its 'active' transfers and
    // spawning a second process_job would put two workers on the same file.
    if state.jobs.lock().unwrap().contains_key(&job_id) {
        return Ok(Json(
            json!({ "ok": false, "note": "job is already running — pause it first to retry" }),
        ));
    }

    let token = get_setting(&state.pool, "rd_token")
        .await
        .ok()
        .flatten()
        .ok_or((
            StatusCode::BAD_REQUEST,
            "Real-Debrid token not set".to_string(),
        ))?;

    let reset = sqlx::query(
        "UPDATE transfers SET status='queued', error=NULL, updated_at=datetime('now')
         WHERE status IN ('error', 'active')
           AND node_id IN (SELECT id FROM nodes WHERE job_id = ?)",
    )
    .bind(&job_id)
    .execute(&state.pool)
    .await
    .map_err(internal_sqlx)?
    .rows_affected();

    sqlx::query("UPDATE jobs SET status='downloading', updated_at=datetime('now') WHERE id=?")
        .bind(&job_id)
        .execute(&state.pool)
        .await
        .map_err(internal_sqlx)?;

    let dest = job_dest(&state.pool, &job_id).await;
    let downloader = build_downloader(&state, token, dest).await;
    spawn_job(state, downloader, job_id);

    Ok(Json(json!({ "ok": true, "requeued": reset })))
}

/// Row shape of the jobs-list aggregate: (id, status, created_at, root_name,
/// total, done, error, bytes_total, bytes_done).
type JobListRow = (String, String, String, Option<String>, i64, i64, i64, i64, i64);

/// List all jobs with aggregate progress, newest first.
pub async fn list_jobs(State(state): State<AppState>) -> Result<Json<Value>, (StatusCode, String)> {
    let rows: Vec<JobListRow> =
        sqlx::query_as(
            "SELECT j.id, j.status, j.created_at,
                    (SELECT name FROM nodes WHERE job_id = j.id AND parent_handle IS NULL LIMIT 1),
                    COUNT(t.id),
                    COALESCE(SUM(t.status = 'done'), 0),
                    COALESCE(SUM(t.status = 'error'), 0),
                    COALESCE(SUM(t.bytes_total), 0),
                    COALESCE(SUM(t.bytes_done), 0)
             FROM jobs j
             LEFT JOIN nodes n ON n.job_id = j.id
             LEFT JOIN transfers t ON t.node_id = n.id
             GROUP BY j.id
             ORDER BY j.created_at DESC",
        )
        .fetch_all(&state.pool)
        .await
        .map_err(internal_sqlx)?;

    let jobs: Vec<Value> = rows
        .into_iter()
        .map(
            |(id, status, created_at, root_name, total, done, error, bytes_total, bytes_done)| {
                json!({
                    "id": id, "status": status, "created_at": created_at,
                    "root_name": root_name, "total": total, "done": done, "error": error,
                    "bytes_total": bytes_total, "bytes_done": bytes_done,
                })
            },
        )
        .collect();

    Ok(Json(json!({ "jobs": jobs })))
}

/// Return a job's full reconstructed tree (from the DB, not MEGA) plus the
/// current per-file transfer state — enough to render and resume in the UI
/// after a page reload.
pub async fn get_job(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let job: Option<(String, String)> =
        sqlx::query_as("SELECT status, link FROM jobs WHERE id = ?")
            .bind(&job_id)
            .fetch_optional(&state.pool)
            .await
            .map_err(internal_sqlx)?;
    let (status, _link) = job.ok_or((StatusCode::NOT_FOUND, "job not found".to_string()))?;

    let node_rows: Vec<(String, Option<String>, String, String, String, i64)> = sqlx::query_as(
        "SELECT handle, parent_handle, kind, name, rel_path, size FROM nodes WHERE job_id = ? ORDER BY rel_path",
    )
    .bind(&job_id)
    .fetch_all(&state.pool)
    .await
    .map_err(internal_sqlx)?;

    let handles: std::collections::HashSet<&str> =
        node_rows.iter().map(|(h, ..)| h.as_str()).collect();

    let mut nodes = Vec::with_capacity(node_rows.len());
    let (mut total_files, mut total_folders, mut total_bytes) = (0i64, 0i64, 0i64);
    let mut root_handle = String::new();
    let mut root_name = String::from("(root)");
    for (handle, parent, kind, name, rel_path, size) in &node_rows {
        let parent_in = parent.as_deref().is_some_and(|p| handles.contains(p));
        if kind == "file" {
            total_files += 1;
            total_bytes += size;
        } else {
            total_folders += 1;
            if !parent_in {
                root_handle = handle.clone();
                root_name = name.clone();
            }
        }
        nodes.push(json!({
            "handle": handle,
            "parent": if parent_in { parent.clone() } else { None },
            "kind": kind, "name": name, "rel_path": rel_path, "size": size,
        }));
    }

    let transfer_rows: Vec<(String, i64, i64, String)> = sqlx::query_as(
        "SELECT n.handle, t.bytes_done, t.bytes_total, t.status
         FROM transfers t JOIN nodes n ON n.id = t.node_id WHERE n.job_id = ?",
    )
    .bind(&job_id)
    .fetch_all(&state.pool)
    .await
    .map_err(internal_sqlx)?;

    let mut transfers = serde_json::Map::new();
    for (handle, bytes_done, bytes_total, st) in transfer_rows {
        transfers.insert(
            handle,
            json!({ "bytes_done": bytes_done, "bytes_total": bytes_total, "status": st }),
        );
    }

    Ok(Json(json!({
        "id": job_id,
        "status": status,
        "tree": {
            "root_handle": root_handle, "root_name": root_name,
            "total_files": total_files, "total_folders": total_folders, "total_bytes": total_bytes,
            "nodes": nodes,
        },
        "transfers": transfers,
    })))
}

/// Pause a running job: cancel its token; in-flight files stop (resumable).
pub async fn pause_job(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Json<Value> {
    let token = state.jobs.lock().unwrap().get(&job_id).cloned();
    match token {
        Some(t) => {
            t.cancel();
            Json(json!({ "ok": true, "paused": true }))
        }
        None => Json(json!({ "ok": true, "paused": false, "note": "job not running" })),
    }
}

/// Resume a paused job (continues queued/partial files).
pub async fn resume_job(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    if state.jobs.lock().unwrap().contains_key(&job_id) {
        return Ok(Json(json!({ "ok": true, "note": "already running" })));
    }
    let exists: Option<String> = sqlx::query_scalar("SELECT id FROM jobs WHERE id = ?")
        .bind(&job_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(internal_sqlx)?;
    if exists.is_none() {
        return Err((StatusCode::NOT_FOUND, "job not found".to_string()));
    }
    let token = get_setting(&state.pool, "rd_token")
        .await
        .ok()
        .flatten()
        .ok_or((StatusCode::BAD_REQUEST, "Real-Debrid token not set".to_string()))?;

    sqlx::query("UPDATE jobs SET status='downloading', updated_at=datetime('now') WHERE id=?")
        .bind(&job_id)
        .execute(&state.pool)
        .await
        .map_err(internal_sqlx)?;

    let dest = job_dest(&state.pool, &job_id).await;
    let downloader = build_downloader(&state, token, dest).await;
    spawn_job(state, downloader, job_id);
    Ok(Json(json!({ "ok": true })))
}

/// Delete a job (and its nodes/transfers via cascade). Downloaded files are
/// left on disk.
pub async fn delete_job(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    if let Some(t) = state.jobs.lock().unwrap().get(&job_id).cloned() {
        t.cancel();
    }
    sqlx::query("DELETE FROM jobs WHERE id = ?")
        .bind(&job_id)
        .execute(&state.pool)
        .await
        .map_err(internal_sqlx)?;
    Ok(Json(json!({ "ok": true })))
}

/// Aggregate progress for a job (for monitoring / the queue UI).
pub async fn job_status(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let job_status: String = sqlx::query_scalar("SELECT status FROM jobs WHERE id = ?")
        .bind(&job_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(internal_sqlx)?
        .ok_or((StatusCode::NOT_FOUND, "job not found".to_string()))?;

    let (total, done, active, errored, bytes_total, bytes_done): (i64, i64, i64, i64, i64, i64) =
        sqlx::query_as(
            "SELECT
               COUNT(*),
               COALESCE(SUM(status = 'done'), 0),
               COALESCE(SUM(status = 'active'), 0),
               COALESCE(SUM(status = 'error'), 0),
               COALESCE(SUM(bytes_total), 0),
               COALESCE(SUM(bytes_done), 0)
             FROM transfers t JOIN nodes n ON n.id = t.node_id
             WHERE n.job_id = ?",
        )
        .bind(&job_id)
        .fetch_one(&state.pool)
        .await
        .map_err(internal_sqlx)?;

    Ok(Json(json!({
        "status": job_status,
        "total": total,
        "done": done,
        "active": active,
        "error": errored,
        "bytes_total": bytes_total,
        "bytes_done": bytes_done,
    })))
}

#[derive(Deserialize)]
pub struct ZipParams {
    /// Optional rel_path prefix to zip just one subfolder of the job.
    #[serde(default)]
    pub prefix: Option<String>,
}

/// Stream a `.zip` of a job's downloaded files (or a subfolder via `?prefix=`),
/// built on the fly with no intermediate temp file. Stored (uncompressed) since
/// the payload is mostly already-compressed media.
pub async fn job_zip(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
    Query(params): Query<ZipParams>,
) -> Result<Response, (StatusCode, String)> {
    let prefix = params.prefix.unwrap_or_default();

    let base: String = sqlx::query_scalar("SELECT root_path FROM jobs WHERE id = ?")
        .bind(&job_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(internal_sqlx)?
        .ok_or((StatusCode::NOT_FOUND, "job not found".to_string()))?;
    let base = PathBuf::from(base);

    // Only fully-downloaded files: an in-flight partial on disk would zip up
    // as a silently truncated (corrupt) entry.
    let rels: Vec<(String,)> = sqlx::query_as(
        "SELECT n.rel_path FROM nodes n
         JOIN transfers t ON t.node_id = n.id
         WHERE n.job_id = ? AND n.kind = 'file' AND t.status = 'done'
         ORDER BY n.rel_path",
    )
    .bind(&job_id)
    .fetch_all(&state.pool)
    .await
    .map_err(internal_sqlx)?;

    // Collect the files that exist on disk and fall under the requested prefix.
    let mut files: Vec<(PathBuf, String)> = Vec::new();
    for (rel,) in rels {
        if !prefix.is_empty() && rel != prefix && !rel.starts_with(&format!("{prefix}/")) {
            continue;
        }
        let abs = engine::download::local_path_in(&base, &rel);
        if tokio::fs::try_exists(&abs).await.unwrap_or(false) {
            files.push((abs, engine::download::archive_name(&rel)));
        }
    }
    if files.is_empty() {
        return Err((
            StatusCode::NOT_FOUND,
            "no downloaded files to zip yet".to_string(),
        ));
    }

    // Name the archive after the subfolder, or the job's root folder.
    let base_name = if !prefix.is_empty() {
        prefix.rsplit('/').next().unwrap_or("download").to_string()
    } else {
        sqlx::query_scalar::<_, String>(
            "SELECT rel_path FROM nodes WHERE job_id = ? ORDER BY length(rel_path) LIMIT 1",
        )
        .bind(&job_id)
        .fetch_optional(&state.pool)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "download".to_string())
    };
    let filename = ascii_filename(&base_name);

    // Build the zip in a task, streaming through an in-memory pipe to the client.
    let (writer, reader) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        if let Err(e) = write_zip(writer, files).await {
            tracing::error!("zip stream for job {job_id} failed: {e}");
        }
    });

    let body = Body::from_stream(ReaderStream::new(reader));
    Response::builder()
        .header(header::CONTENT_TYPE, "application/zip")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{filename}.zip\""),
        )
        .body(body)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

/// Write a stored (uncompressed) zip of `files` into `sink`, streaming each
/// file's bytes so nothing large is buffered in memory.
async fn write_zip(
    sink: tokio::io::DuplexStream,
    files: Vec<(PathBuf, String)>,
) -> anyhow::Result<()> {
    use tokio_util::compat::TokioAsyncReadCompatExt;

    let mut zip = async_zip::tokio::write::ZipFileWriter::with_tokio(sink);
    for (abs, name) in files {
        let entry = async_zip::ZipEntryBuilder::new(name.into(), async_zip::Compression::Stored);
        let mut entry_writer = zip.write_entry_stream(entry).await?;
        // async_zip's entry writer is a futures AsyncWrite; bridge the tokio File.
        let mut file = tokio::fs::File::open(&abs).await?.compat();
        futures_lite::io::copy(&mut file, &mut entry_writer).await?;
        entry_writer.close().await?;
    }
    zip.close().await?;
    Ok(())
}

/// Reduce a name to a safe ASCII filename for the Content-Disposition header.
fn ascii_filename(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c.is_ascii() && c != '"' && c != '\\' && (c as u32) >= 0x20 {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.trim().is_empty() {
        "download".to_string()
    } else {
        cleaned
    }
}

/// Spawn a job's processing in the background under a cancellation token (so it
/// can be paused), registering it in the running-jobs map for its lifetime.
///
/// Refuses (returns `false`) if the job is already running: two concurrent
/// `process_job` runs would race their workers over the same files on disk.
pub fn spawn_job(state: AppState, downloader: Downloader, job_id: String) -> bool {
    let cancel = CancellationToken::new();
    {
        let mut jobs = state.jobs.lock().unwrap();
        if jobs.contains_key(&job_id) {
            return false;
        }
        jobs.insert(job_id.clone(), cancel.clone());
    }
    tokio::spawn(async move {
        if let Err(e) = downloader.process_job(&job_id, cancel).await {
            tracing::error!("job {job_id} failed: {e}");
        }
        state.jobs.lock().unwrap().remove(&job_id);
    });
    true
}

// --- Aria2 / path mapping ---------------------------------------------------

/// Load the effective Aria2 config from settings + env. Returns `None` when no
/// RPC URL is configured (Aria2 backend disabled → built-in streamer).
pub async fn aria2_config(pool: &SqlitePool) -> Option<engine::aria2::AriaConfig> {
    // Env vars seed the defaults; the DB (UI) overrides them.
    let url = get_setting(pool, "aria2_rpc_url")
        .await
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("ARIA2_RPC_URL").ok())
        .and_then(|s| {
            let s = s.trim().to_string();
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        })?;

    let secret = get_setting(pool, "aria2_rpc_secret")
        .await
        .ok()
        .flatten()
        .or_else(|| std::env::var("ARIA2_SECRET").ok())
        .or_else(|| std::env::var("ARIA2_RPC_SECRET").ok());
    let speed = get_setting(pool, "max_download_speed")
        .await
        .ok()
        .flatten()
        .or_else(|| std::env::var("MAX_DOWNLOAD_SPEED").ok())
        .filter(|s| !s.trim().is_empty());

    Some(engine::aria2::AriaConfig {
        rpc_url: Some(url),
        secret,
        // Default cap when unset: 5 MiB/s (editable in the UI).
        max_download_limit: Some(speed.unwrap_or_else(|| "5M".to_string())),
    })
}

/// Load all path mappings ordered by position.
pub async fn path_mappings_all(pool: &SqlitePool) -> Vec<engine::PathMapping> {
    sqlx::query_as::<_, (i64, String, String, i64)>(
        "SELECT id, remote_path, local_path, position FROM path_mappings ORDER BY position, id",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|(id, remote_path, local_path, position)| engine::PathMapping {
        id: Some(id),
        remote_path,
        local_path,
        position,
    })
    .collect()
}

// --- settings helpers -------------------------------------------------------

pub async fn get_setting(pool: &SqlitePool, key: &str) -> Result<Option<String>, sqlx::Error> {
    let row: Option<(String,)> = sqlx::query_as("SELECT value FROM settings WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|(v,)| v))
}

async fn set_setting(pool: &SqlitePool, key: &str, value: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO settings (key, value) VALUES (?, ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}

/// Set a setting, or remove it entirely when the new value is empty (so the
/// engine falls back to its default instead of being stuck forever).
async fn upsert_or_clear(pool: &SqlitePool, key: &str, value: &str) -> Result<(), sqlx::Error> {
    if value.is_empty() {
        sqlx::query("DELETE FROM settings WHERE key = ?")
            .bind(key)
            .execute(pool)
            .await?;
        Ok(())
    } else {
        set_setting(pool, key, value).await
    }
}

// --- error mapping ----------------------------------------------------------

fn bad_request(e: engine::Error) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, e.to_string())
}

fn internal(e: serde_json::Error) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

fn internal_sqlx(e: sqlx::Error) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}
