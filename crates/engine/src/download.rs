//! The download engine.
//!
//! Per file: build its per-node MEGA link → unrestrict via Real-Debrid → stream
//! the bytes into the correct nested folder on disk. State (job/nodes/transfers)
//! is persisted to SQLite so downloads resume after a crash or restart, and
//! existing partial files are continued via HTTP range requests.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aes::cipher::generic_array::GenericArray;
use async_channel::Sender;
use ctr::cipher::{KeyIvInit, StreamCipher};
use futures::stream::StreamExt;
use reqwest::header::RANGE;
use sqlx::SqlitePool;
use tokio::io::AsyncWriteExt;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::events::EngineEvent;
use crate::aria2::{AddUriOptions, Aria2Client, AriaConfig};
use crate::mega::{self, crypto, MegaLink, NodeKind, Tree};
use crate::realdebrid::RealDebrid;
use crate::PathMapping;
use crate::{Error, Result};

/// MEGA encrypts file contents with AES-128 in CTR mode (big-endian counter).
type Aes128Ctr = ctr::Ctr128BE<aes::Aes128>;

/// Default number of files downloaded in parallel (overridable via settings).
pub const DEFAULT_CONCURRENCY: usize = 4;
/// How often to persist/emit progress for an active file.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(300);

/// Row shape of a pending transfer joined with its node:
/// (transfer_id, handle, rel_path, size, file_key).
type PendingRow = (String, String, String, i64, Option<Vec<u8>>);

/// A unit of work in the per-job download queue.
struct FileTask {
    transfer_id: String,
    handle: String,
    rel_path: String,
    size: i64,
    /// 32-byte MEGA file key for the native fallback (None for nodes without one).
    file_key: Option<Vec<u8>>,
    /// How many transient RD failures this file has had so far.
    attempt: u32,
}

#[derive(Clone)]
pub struct Downloader {
    pool: SqlitePool,
    events: broadcast::Sender<EngineEvent>,
    rd: RealDebrid,
    dest_root: PathBuf,
    concurrency: usize,
    /// Optional Aria2 backend (rate-limited downloads + AriaNg visibility).
    aria2: Option<Aria2Client>,
    /// Path mappings used to translate Aria2-reported paths to local paths.
    path_mappings: std::sync::Arc<std::sync::RwLock<Vec<PathMapping>>>,
    /// Default per-download speed limit (aria2 format, e.g. "5M", "0"=unlimited).
    max_download_speed: Option<String>,
}

impl Downloader {
    pub fn new(
        pool: SqlitePool,
        events: broadcast::Sender<EngineEvent>,
        rd: RealDebrid,
        dest_root: PathBuf,
        concurrency: usize,
    ) -> Self {
        Self {
            pool,
            events,
            rd,
            dest_root,
            concurrency: concurrency.clamp(1, 16),
            aria2: None,
            path_mappings: std::sync::Arc::new(std::sync::RwLock::new(Vec::new())),
            max_download_speed: None,
        }
    }

    /// Configure the Aria2 backend. Pass `None` to keep using the built-in
    /// streamer for everything.
    pub fn with_aria2(&mut self, cfg: &AriaConfig) -> &mut Self {
        self.aria2 = Aria2Client::from_config(cfg);
        self.max_download_speed = cfg.max_download_limit.clone();
        self
    }

    /// Set the remote→local path mappings (for moving Aria2-completed files).
    pub fn set_path_mappings(&mut self, mappings: Vec<PathMapping>) -> &mut Self {
        *self.path_mappings.write().unwrap() = mappings;
        self
    }

    /// True when an Aria2 client is configured (RD downloads go through it).
    pub fn aria2_enabled(&self) -> bool {
        self.aria2.is_some()
    }

    fn emit(&self, ev: EngineEvent) {
        // Ignore send errors: a broadcast with no current subscribers is fine.
        let _ = self.events.send(ev);
    }

    /// List + persist the tree and create one transfer per (selected) file.
    /// `include` optionally restricts which file handles get queued.
    pub async fn create_job(
        &self,
        link_str: &str,
        include: Option<Vec<String>>,
    ) -> Result<(String, Tree)> {
        let link = mega::parse(link_str)?;
        let tree = mega::fetch_tree(&link).await?;

        let job_id = Uuid::new_v4().to_string();
        let root_path = self.dest_root.to_string_lossy().to_string();

        // One transaction for the job + all its rows: a crash mid-create can't
        // leave a partial job that startup would later resume as if complete
        // (and batching the inserts is much faster for large folders).
        let mut tx = self.pool.begin().await?;

        sqlx::query(
            "INSERT INTO jobs (id, link, kind, root_path, transport, status)
             VALUES (?, ?, 'folder', ?, 'realdebrid', 'downloading')",
        )
        .bind(&job_id)
        .bind(link_str)
        .bind(&root_path)
        .execute(&mut *tx)
        .await?;

        let include: Option<HashSet<String>> = include.map(|v| v.into_iter().collect());

        for node in &tree.nodes {
            let node_id = Uuid::new_v4().to_string();
            let kind = match node.kind {
                NodeKind::File => "file",
                NodeKind::Folder => "folder",
            };
            sqlx::query(
                "INSERT INTO nodes (id, job_id, parent_handle, handle, kind, name, rel_path, size, file_key)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&node_id)
            .bind(&job_id)
            .bind(&node.parent)
            .bind(&node.handle)
            .bind(kind)
            .bind(&node.name)
            .bind(&node.rel_path)
            .bind(node.size)
            .bind(node.file_key.as_ref().map(|k| k.to_vec()))
            .execute(&mut *tx)
            .await?;

            let selected = include
                .as_ref()
                .is_none_or(|set| set.contains(&node.handle));
            if node.kind == NodeKind::File && selected {
                let transfer_id = Uuid::new_v4().to_string();
                sqlx::query(
                    "INSERT INTO transfers (id, node_id, status, bytes_total)
                     VALUES (?, ?, 'queued', ?)",
                )
                .bind(&transfer_id)
                .bind(&node_id)
                .bind(node.size)
                .execute(&mut *tx)
                .await?;
            }
        }

        tx.commit().await?;

        self.emit(EngineEvent::JobCreated {
            job_id: job_id.clone(),
            root_name: tree.root_name.clone(),
            total_files: tree.total_files,
            total_bytes: tree.total_bytes,
        });

        Ok((job_id, tree))
    }

    /// Download all not-yet-done transfers for a job (resumable & idempotent).
    /// Cancelling `cancel` pauses the job: in-flight files stop (leaving partial
    /// files to resume) and the job is marked `paused`.
    pub async fn process_job(&self, job_id: &str, cancel: CancellationToken) -> Result<()> {
        let (link_str,): (String,) = sqlx::query_as("SELECT link FROM jobs WHERE id = ?")
            .bind(job_id)
            .fetch_one(&self.pool)
            .await?;

        let (folder_id, folder_key) = match mega::parse(&link_str)? {
            MegaLink::Folder { id, key, .. } => (id, key),
            MegaLink::File { .. } => return Err(Error::Other("job link is not a folder".into())),
        };

        // Crash recovery: any transfer left 'active' from a previous run was
        // interrupted — requeue it so it gets picked up (and resumed) below.
        sqlx::query(
            "UPDATE transfers SET status='queued'
             WHERE status='active' AND node_id IN (SELECT id FROM nodes WHERE job_id = ?)",
        )
        .bind(job_id)
        .execute(&self.pool)
        .await?;

        // Pending transfers joined with their node info (incl. the file key for
        // the native fallback).
        let rows: Vec<PendingRow> = sqlx::query_as(
            "SELECT t.id, n.handle, n.rel_path, n.size, n.file_key
             FROM transfers t JOIN nodes n ON n.id = t.node_id
             WHERE n.job_id = ? AND t.status <> 'done'",
        )
        .bind(job_id)
        .fetch_all(&self.pool)
        .await?;

        let tasks: Vec<FileTask> = rows
            .into_iter()
            .map(|(transfer_id, handle, rel_path, size, file_key)| FileTask {
                transfer_id,
                handle,
                rel_path,
                size,
                file_key,
                attempt: 0,
            })
            .collect();

        self.run_queue(job_id, &folder_id, &folder_key, tasks, &cancel)
            .await;

        // Paused: requeue what was in flight (partial files resume later) so
        // nothing is left showing 'active' while the job sits paused.
        if cancel.is_cancelled() {
            sqlx::query(
                "UPDATE transfers SET status='queued', updated_at=datetime('now')
                 WHERE status='active' AND node_id IN (SELECT id FROM nodes WHERE job_id = ?)",
            )
            .bind(job_id)
            .execute(&self.pool)
            .await?;
            sqlx::query("UPDATE jobs SET status='paused', updated_at=datetime('now') WHERE id=?")
                .bind(job_id)
                .execute(&self.pool)
                .await?;
            self.emit(EngineEvent::JobDone {
                job_id: job_id.to_string(),
            });
            return Ok(());
        }

        // Mark the job done only if nothing remains.
        let (remaining,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM transfers t JOIN nodes n ON n.id = t.node_id
             WHERE n.job_id = ? AND t.status <> 'done'",
        )
        .bind(job_id)
        .fetch_one(&self.pool)
        .await?;

        let status = if remaining == 0 { "done" } else { "error" };
        sqlx::query("UPDATE jobs SET status=?, updated_at=datetime('now') WHERE id=?")
            .bind(status)
            .bind(job_id)
            .execute(&self.pool)
            .await?;

        self.emit(EngineEvent::JobDone {
            job_id: job_id.to_string(),
        });
        Ok(())
    }

    /// Run the per-job download queue: a fixed pool of `CONCURRENCY` workers
    /// pulling from a shared channel. Crucially, a file that needs to wait
    /// (backoff after a transient failure) is *not* held in a worker slot —
    /// it's re-queued by a detached timer so the slot serves another file
    /// immediately. This keeps the pipe full while cold-cache files warm up.
    async fn run_queue(
        &self,
        job_id: &str,
        folder_id: &str,
        folder_key: &str,
        tasks: Vec<FileTask>,
        cancel: &CancellationToken,
    ) {
        if tasks.is_empty() {
            return;
        }

        let (tx, rx) = async_channel::unbounded::<FileTask>();
        let remaining = Arc::new(AtomicUsize::new(tasks.len()));
        for task in tasks {
            let _ = tx.send(task).await;
        }

        let mut workers = Vec::with_capacity(self.concurrency);
        for _ in 0..self.concurrency {
            let dl = self.clone();
            let rx = rx.clone();
            let tx = tx.clone();
            let remaining = remaining.clone();
            let cancel = cancel.clone();
            let job_id = job_id.to_string();
            let folder_id = folder_id.to_string();
            let folder_key = folder_key.to_string();
            workers.push(tokio::spawn(async move {
                loop {
                    let task = tokio::select! {
                        biased;
                        _ = cancel.cancelled() => break,
                        recv = rx.recv() => match recv {
                            Ok(t) => t,
                            Err(_) => break, // channel closed = all work done
                        },
                    };
                    dl.handle_task(&job_id, &folder_id, &folder_key, task, &tx, &remaining, &cancel)
                        .await;
                }
            }));
        }
        drop(tx);
        drop(rx);
        for worker in workers {
            let _ = worker.await;
        }
    }

    /// Process one file attempt: download via RD; on a transient failure, defer
    /// it (re-queue after a backoff) without blocking the worker; once RD
    /// retries are exhausted (or fail permanently), fall back to native MEGA.
    #[allow(clippy::too_many_arguments)]
    async fn handle_task(
        &self,
        job_id: &str,
        folder_id: &str,
        folder_key: &str,
        mut task: FileTask,
        tx: &Sender<FileTask>,
        remaining: &Arc<AtomicUsize>,
        cancel: &CancellationToken,
    ) {
        let path = self.local_path(&task.rel_path);
        if let Some(parent) = path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }

        // Fast path: a complete file already on disk.
        if task.size > 0 && file_len(&path).await == task.size {
            let _ = self
                .finish(job_id, &task.transfer_id, &task.handle, task.size)
                .await;
            self.terminal(remaining, tx);
            return;
        }

        let _ =
            sqlx::query("UPDATE transfers SET status='active', updated_at=datetime('now') WHERE id=?")
                .bind(&task.transfer_id)
                .execute(&self.pool)
                .await;

        let link = format!(
            "https://mega.nz/folder/{folder_id}#{folder_key}/file/{}",
            task.handle
        );
        let result = if self.aria2_enabled() {
            self.download_rd_via_aria2(job_id, &task, &link, &path, cancel)
                .await
        } else {
            self.try_download(job_id, &task.transfer_id, &task.handle, &link, &path, task.size, cancel)
                .await
        };

        let err = match result {
            Ok(()) => {
                self.terminal(remaining, tx);
                return;
            }
            Err(Error::Cancelled) => return, // paused: leave for resume, don't finalize
            Err(e) => e,
        };

        // Transient failure with attempts left → defer (free the slot now).
        let policy = classify(&err);
        let max = policy.max_attempts();
        if policy != RetryPolicy::Fatal && task.attempt + 1 < max {
            task.attempt += 1;
            let delay = policy.backoff(task.attempt);
            let _ = sqlx::query(
                "UPDATE transfers SET status='queued', retries=?, error=?, updated_at=datetime('now') WHERE id=?",
            )
            .bind(task.attempt as i64)
            .bind(err.to_string())
            .bind(&task.transfer_id)
            .execute(&self.pool)
            .await;
            self.emit(EngineEvent::FileRetry {
                job_id: job_id.into(),
                handle: task.handle.clone(),
                attempt: task.attempt,
                max,
                reason: err.to_string(),
            });
            let tx = tx.clone();
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                let _ = tx.send(task).await;
            });
            return;
        }

        // RD exhausted / permanent → native MEGA fallback if we have the key.
        if let Some(key) = task.file_key.as_deref().filter(|k| k.len() >= 32) {
            tracing::warn!(
                "Real-Debrid exhausted for {} ({err}); falling back to native MEGA",
                task.handle
            );
            self.emit(EngineEvent::FileFallback {
                job_id: job_id.into(),
                handle: task.handle.clone(),
            });
            let mut key32 = [0u8; 32];
            key32.copy_from_slice(&key[..32]);
            match self
                .download_via_native(
                    job_id,
                    &task.transfer_id,
                    &task.handle,
                    folder_id,
                    &key32,
                    &path,
                    task.size,
                    cancel,
                )
                .await
            {
                Ok(()) => {}
                Err(Error::Cancelled) => return,
                Err(ne) => self.fail(job_id, &task.transfer_id, &task.handle, &ne).await,
            }
        } else {
            self.fail(job_id, &task.transfer_id, &task.handle, &err).await;
        }
        self.terminal(remaining, tx);
    }

    /// Record one finished task; close the queue once none remain.
    fn terminal(&self, remaining: &Arc<AtomicUsize>, tx: &Sender<FileTask>) {
        if remaining.fetch_sub(1, Ordering::SeqCst) == 1 {
            tx.close();
        }
    }

    /// Mark a transfer failed and emit the error event.
    async fn fail(&self, job_id: &str, transfer_id: &str, handle: &str, e: &Error) {
        let _ = sqlx::query(
            "UPDATE transfers SET status='error', error=?, updated_at=datetime('now') WHERE id=?",
        )
        .bind(e.to_string())
        .bind(transfer_id)
        .execute(&self.pool)
        .await;
        self.emit(EngineEvent::FileError {
            job_id: job_id.into(),
            handle: handle.into(),
            error: e.to_string(),
        });
    }

    /// Map a node's relative path to a sanitized absolute destination path.
    fn local_path(&self, rel_path: &str) -> PathBuf {
        local_path_in(&self.dest_root, rel_path)
    }

    /// Download directly from MEGA's CDN and AES-CTR-decrypt the content on the
    /// fly. No resume (these are rare fallbacks); the file is written fresh.
    #[allow(clippy::too_many_arguments)]
    async fn download_via_native(
        &self,
        job_id: &str,
        transfer_id: &str,
        handle: &str,
        folder_id: &str,
        file_key32: &[u8; 32],
        path: &Path,
        expected: i64,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let (url, size) = mega::folder::fetch_download_url(folder_id, handle).await?;
        let total = if expected > 0 { expected } else { size };

        // Derive the AES-CTR key + IV: key = folded 16-byte key; IV = the 8-byte
        // nonce (bytes 16..24 of the node key) followed by a zero block counter.
        let aes_key = crypto::unpack_file_key(file_key32);
        let mut iv = [0u8; 16];
        iv[..8].copy_from_slice(&file_key32[16..24]);
        let mut cipher = Aes128Ctr::new(GenericArray::from_slice(&aes_key), GenericArray::from_slice(&iv));

        let resp = self.rd.client().get(&url).send().await?.error_for_status()?;
        let mut file = tokio::fs::File::create(path).await?;
        let mut downloaded: i64 = 0;
        let mut last = Instant::now();
        let mut stream = resp.bytes_stream();
        loop {
            // Select on the cancel token so pause works even when the
            // connection has stalled and no chunk ever arrives.
            let chunk = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(Error::Cancelled),
                chunk = stream.next() => match chunk {
                    Some(c) => c?,
                    None => break,
                },
            };
            let mut buf = chunk.to_vec();
            cipher.apply_keystream(&mut buf);
            file.write_all(&buf).await?;
            downloaded += buf.len() as i64;
            if last.elapsed() >= PROGRESS_INTERVAL {
                self.persist_progress(transfer_id, downloaded).await;
                self.emit(EngineEvent::Progress {
                    job_id: job_id.into(),
                    handle: handle.into(),
                    bytes_done: downloaded,
                    bytes_total: total,
                });
                last = Instant::now();
            }
        }
        file.flush().await?;

        if total > 0 && downloaded != total {
            return Err(Error::Incomplete {
                got: downloaded,
                expected: total,
            });
        }

        self.emit(EngineEvent::Progress {
            job_id: job_id.into(),
            handle: handle.into(),
            bytes_done: downloaded,
            bytes_total: total,
        });
        self.finish(job_id, transfer_id, handle, downloaded).await?;
        Ok(())
    }

    /// Route a Real-Debrid download through Aria2 (rate-limited, visible in
    /// AriaNg). The bytes land flat in Aria2's download area; when Aria2
    /// reports completion we translate its path with the remote path mapping
    /// and move the file into the correct nested MEGA folder (rename same-fs,
    /// copy+remove cross-fs).
    #[allow(clippy::too_many_arguments)]
    async fn download_rd_via_aria2(
        &self,
        job_id: &str,
        task: &FileTask,
        link: &str,
        final_path: &Path,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let aria2 = self.aria2.as_ref().expect("aria2 enabled");

        // Fast path: final file already on disk.
        if task.size > 0 && file_len(final_path).await == task.size {
            return self
                .finish(job_id, &task.transfer_id, &task.handle, task.size)
                .await;
        }

        // 1. Unrestrict the Real-Debrid link (gives a short-lived plaintext URL).
        let unrestricted = self.rd.unrestrict(link).await?;
        let total = if unrestricted.filesize > 0 {
            unrestricted.filesize
        } else {
            task.size
        };

        // 2. Work out where Aria2 should write the file. We keep it inside the
        //    job's sub-dir under Aria2's root; the remote→local mapping later
        //    tells us where that is on this filesystem.
        let aria2_root = self.aria2_root();
        let filename = task
            .rel_path
            .split('/')
            .filter(|s| !s.is_empty())
            .next_back()
            .map(sanitize_segment)
            .unwrap_or_else(|| format!("{}.part", task.handle));
        let aria2_dir = PathBuf::from(&aria2_root).join(format!("job-{}", &job_id[..8.min(job_id.len())]));

        // Existing partial (Aria2 uses .aria2 control files for resume, so a
        // prior attempt just resumes; we only modify the DB state).
        let _ = sqlx::query(
            "UPDATE transfers SET status='active', source='aria2', updated_at=datetime('now') WHERE id=?",
        )
        .bind(&task.transfer_id)
        .execute(&self.pool)
        .await;

        let opts = AddUriOptions {
            dir: aria2_dir.to_string_lossy().to_string(),
            max_download_limit: self.max_download_speed.clone(),
            max_connection_per_server: Some("1".into()), // RD limits per-conn
            continue_field: Some("true".into()),
            title: Some(task.rel_path.clone()),
            ..Default::default()
        };
        let gid = aria2
            .add_uri(&[unrestricted.download.clone()], opts)
            .await?;
        let _ = sqlx::query("UPDATE transfers SET aria2_gid=?, updated_at=datetime('now') WHERE id=?")
            .bind(&gid)
            .bind(&task.transfer_id)
            .execute(&self.pool)
            .await;

        // 3. Poll until Aria2 finishes (or we're cancelled / the job is paused).
        let downloaded = self.poll_aria2_file(job_id, task, &gid, total, cancel).await?;

        // 4. Move Aria2's file into the correct nested MEGA folder.
        let finished_path = {
            let status = aria2.tell_status(&gid).await?;
            status
                .files
                .first()
                .map(|f| f.path.clone())
                .filter(|p| !p.is_empty())
                .unwrap_or_else(|| aria2_dir.join(&filename).to_string_lossy().to_string())
        };
        let local_src = self.map_aria2_path(&finished_path);
        self.ensure_parents(final_path).await;
        move_file(&local_src, final_path).await?;

        self.emit(EngineEvent::Progress {
            job_id: job_id.into(),
            handle: task.handle.clone(),
            bytes_done: downloaded,
            bytes_total: total.max(downloaded),
        });
        self.finish(job_id, &task.transfer_id, &task.handle, downloaded)
            .await?;
        Ok(())
    }

    /// The directory Aria2 downloads into, as the *remote* (Aria2-side) path.
    /// The first configured path mapping gives the local equivalent; if no
    /// mapping is set we assume Aria2 and this engine share the download dir.
    fn aria2_root(&self) -> String {
        let mappings = self.path_mappings.read().unwrap();
        mappings
            .first()
            .map(|m| m.remote_path.clone())
            .unwrap_or_else(|| self.dest_root.to_string_lossy().to_string())
    }

    /// Translate an Aria2-reported absolute path to a local path this engine
    /// can open, using the configured remote path mappings.
    fn map_aria2_path(&self, aria2_path: &str) -> PathBuf {
        let mappings = self.path_mappings.read().unwrap();
        crate::pathmap::resolve_mapping(&mappings, aria2_path).unwrap_or_else(|| {
            PathBuf::from(aria2_path)
        })
    }

    /// Create parent dirs for a destination path (best-effort).
    async fn ensure_parents(&self, path: &Path) {
        if let Some(parent) = path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
    }

    /// Poll a single Aria2 gid until it completes or errors (or we're
    /// cancelled). Returns bytes downloaded.
    async fn poll_aria2_file(
        &self,
        job_id: &str,
        task: &FileTask,
        gid: &str,
        total: i64,
        cancel: &CancellationToken,
    ) -> Result<i64> {
        let aria2 = self.aria2.as_ref().expect("aria2 enabled");
        let mut last_progress = Instant::now();
        loop {
            let status = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    // Paused/stopped: leave Aria2 running in place; it will
                    // resume next time. Mark transfer paused for the UI.
                    let _ = sqlx::query("UPDATE transfers SET status='paused', updated_at=datetime('now') WHERE id=?")
                        .bind(&task.transfer_id).execute(&self.pool).await;
                    return Err(Error::Cancelled);
                }
                s = aria2.tell_status(gid) => match s {
                    Ok(st) => st,
                    Err(_) => {
                        // Transient RPC hiccup — retry after a short pause.
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        continue;
                    }
                },
            };

            let done = status.completed_length.parse::<i64>().unwrap_or(0);
            match status.status.as_str() {
                "complete" => {
                    return Ok(done);
                }
                "error" => {
                    let code = status.error_code.unwrap_or_default();
                    let msg = base64_err(&status.error_message.unwrap_or_default()); // Aria2 base64-encodes these
                    return Err(Error::Other(format!(
                        "aria2 error (code {code}): {msg}"
                    )));
                }
                "removed" => return Err(Error::Other("aria2 download removed".into())),
                _ => {
                    // active / waiting / paused — report progress periodically.
                    if last_progress.elapsed() >= PROGRESS_INTERVAL {
                        self.persist_progress(&task.transfer_id, done).await;
                        self.emit(EngineEvent::Progress {
                            job_id: job_id.into(),
                            handle: task.handle.clone(),
                            bytes_done: done,
                            bytes_total: total.max(done),
                        });
                        last_progress = Instant::now();
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
        }
    }

    /// A single download attempt: unrestrict, stream (resuming if a partial file
    /// exists), then verify the final size. Returns `Err` for the caller to
    /// classify and possibly retry.
    #[allow(clippy::too_many_arguments)]
    async fn try_download(
        &self,
        job_id: &str,
        transfer_id: &str,
        handle: &str,
        link: &str,
        path: &Path,
        expected: i64,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let mut existing = file_len(path).await;
        if expected > 0 && existing > expected {
            existing = 0; // oversized/corrupt — restart cleanly
            tokio::fs::remove_file(path).await.ok();
        }

        let unrestricted = self.rd.unrestrict(link).await?;
        let total = if unrestricted.filesize > 0 {
            unrestricted.filesize
        } else {
            expected
        };
        let target = if expected > 0 { expected } else { total };

        let mut req = self.rd.client().get(&unrestricted.download);
        if existing > 0 {
            req = req.header(RANGE, format!("bytes={existing}-"));
        }
        let resp = req.send().await?.error_for_status()?;
        // If we asked to resume but the server sent the whole file (200), start over.
        let resumed = existing > 0 && resp.status().as_u16() == 206;

        let mut file = if resumed {
            tokio::fs::OpenOptions::new().append(true).open(path).await?
        } else {
            tokio::fs::File::create(path).await?
        };

        let mut downloaded = if resumed { existing } else { 0 };
        let mut last = Instant::now();
        let mut stream = resp.bytes_stream();
        loop {
            // Select on the cancel token so pause works even when the
            // connection has stalled and no chunk ever arrives.
            let bytes = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(Error::Cancelled),
                chunk = stream.next() => match chunk {
                    Some(c) => c?,
                    None => break,
                },
            };
            file.write_all(&bytes).await?;
            downloaded += bytes.len() as i64;
            if last.elapsed() >= PROGRESS_INTERVAL {
                self.persist_progress(transfer_id, downloaded).await;
                self.emit(EngineEvent::Progress {
                    job_id: job_id.into(),
                    handle: handle.into(),
                    bytes_done: downloaded,
                    bytes_total: total,
                });
                last = Instant::now();
            }
        }
        file.flush().await?;

        // Integrity: a short read means a truncated/dropped transfer — retryable.
        if target > 0 && downloaded != target {
            return Err(Error::Incomplete {
                got: downloaded,
                expected: target,
            });
        }

        self.emit(EngineEvent::Progress {
            job_id: job_id.into(),
            handle: handle.into(),
            bytes_done: downloaded,
            bytes_total: total,
        });
        self.finish(job_id, transfer_id, handle, downloaded).await?;
        Ok(())
    }

    /// Mark a transfer done and emit the completion event.
    async fn finish(&self, job_id: &str, transfer_id: &str, handle: &str, bytes: i64) -> Result<()> {
        self.mark_done(transfer_id, bytes).await?;
        self.emit(EngineEvent::FileDone {
            job_id: job_id.into(),
            handle: handle.into(),
        });
        Ok(())
    }

    /// Persist byte progress. Best-effort: a transient DB hiccup (e.g. a lock
    /// under heavy contention) must not kill an otherwise healthy transfer.
    async fn persist_progress(&self, transfer_id: &str, bytes: i64) {
        let res = sqlx::query("UPDATE transfers SET bytes_done=?, updated_at=datetime('now') WHERE id=?")
            .bind(bytes)
            .bind(transfer_id)
            .execute(&self.pool)
            .await;
        if let Err(e) = res {
            tracing::warn!("progress persist failed for transfer {transfer_id}: {e}");
        }
    }

    async fn mark_done(&self, transfer_id: &str, bytes: i64) -> Result<()> {
        sqlx::query(
            "UPDATE transfers SET status='done', bytes_done=?, error=NULL, updated_at=datetime('now') WHERE id=?",
        )
        .bind(bytes)
        .bind(transfer_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

/// How to react to a failed download attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryPolicy {
    /// Long backoff: Real-Debrid cold-cache (`unavailable_file`, code 24) can
    /// take a while to warm for large files.
    Patient,
    /// Short backoff: dropped connections, rate limits, truncated reads.
    Normal,
    /// Permanent — surface to the user immediately.
    Fatal,
}

impl RetryPolicy {
    fn max_attempts(self) -> u32 {
        match self {
            RetryPolicy::Patient => 8,
            RetryPolicy::Normal => 5,
            RetryPolicy::Fatal => 1,
        }
    }

    fn backoff(self, attempt: u32) -> Duration {
        match self {
            RetryPolicy::Patient => {
                let secs = match attempt {
                    1 => 5,
                    2 => 15,
                    3 => 30,
                    _ => 60,
                };
                Duration::from_secs(secs)
            }
            RetryPolicy::Normal => {
                let secs = (1u64 << attempt.min(5)).min(30); // 2,4,8,16,30
                Duration::from_secs(secs)
            }
            RetryPolicy::Fatal => Duration::ZERO,
        }
    }
}

/// Decide whether (and how patiently) a failed attempt should be retried.
fn classify(e: &Error) -> RetryPolicy {
    match e {
        Error::RealDebrid { status, code, .. } => {
            if *code == Some(24) {
                RetryPolicy::Patient // unavailable_file — cold cache
            } else if *status == 429 || (500..=599).contains(status) {
                RetryPolicy::Normal // rate limit / server error
            } else {
                RetryPolicy::Fatal // unsupported, removed, auth, etc.
            }
        }
        // Network/timeout/stream interruptions — resume and retry.
        Error::Http(_) => RetryPolicy::Normal,
        // Truncated/short transfer — resume the rest.
        Error::Incomplete { .. } => RetryPolicy::Normal,
        // SQLite contention ("database is locked") is transient, not a reason
        // to declare the file dead.
        Error::Db(_) => RetryPolicy::Normal,
        // Disk errors, bad links, etc. need user attention.
        _ => RetryPolicy::Fatal,
    }
}

/// Current size of a file, or 0 if it doesn't exist.
async fn file_len(path: &Path) -> i64 {
    tokio::fs::metadata(path)
        .await
        .map(|m| m.len() as i64)
        .unwrap_or(0)
}

/// Move (or copy+remove) `src` to `dst`. A plain rename is atomic and instant
/// on the same filesystem; if the two paths are on different filesystems
/// (cross-fs rename fails with EXDEV) we copy then remove the source.
async fn move_file(src: &Path, dst: &Path) -> Result<()> {
    match tokio::fs::rename(src, dst).await {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(18 /* EXDEV */) => {
            tracing::warn!("cross-filesystem move {src:?} -> {dst:?}; copying");
            let _ = tokio::fs::remove_file(dst).await;
            tokio::fs::copy(src, dst).await.map_err(Error::Io)?;
            tokio::fs::remove_file(src).await.map_err(Error::Io)?;
            Ok(())
        }
        Err(e) => Err(Error::Io(e)),
    }
}

/// Aria2 base64-encodes `error_message`/`error_code` fields; decode best-effort.
fn base64_err(s: &str) -> String {
    if s.is_empty() {
        return s.to_string();
    }
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .unwrap_or_else(|| s.to_string())
}

/// Map a tree `rel_path` to its sanitized on-disk path under `dest_root`.
/// Shared by the downloader and the zip exporter so they agree on filenames.
pub fn local_path_in(dest_root: &Path, rel_path: &str) -> PathBuf {
    let mut path = dest_root.to_path_buf();
    for seg in rel_path.split('/').filter(|s| !s.is_empty()) {
        path.push(sanitize_segment(seg));
    }
    to_long_path(path)
}

/// On Windows, paths past MAX_PATH (260) fail to open unless given the
/// extended-length `\\?\` form. Deep MEGA trees hit this easily, so convert
/// when needed (drive-letter paths only; anything exotic is left untouched).
#[cfg(windows)]
fn to_long_path(path: PathBuf) -> PathBuf {
    use std::path::{Component, Prefix};

    if path.as_os_str().len() < 260 {
        return path;
    }
    let Ok(abs) = std::path::absolute(&path) else {
        return path;
    };
    let is_plain_disk = matches!(
        abs.components().next(),
        Some(Component::Prefix(p)) if matches!(p.kind(), Prefix::Disk(_))
    );
    if !is_plain_disk {
        return path; // already \\?\-prefixed, or UNC — leave alone
    }
    // Rebuild from components to normalize separators (`\\?\` paths must use
    // backslashes and contain no `.`/`..` segments).
    let normalized: PathBuf = abs.components().collect();
    let mut s = std::ffi::OsString::from(r"\\?\");
    s.push(normalized.as_os_str());
    PathBuf::from(s)
}

#[cfg(not(windows))]
fn to_long_path(path: PathBuf) -> PathBuf {
    path
}

/// The sanitized, `/`-joined name to use for a file inside a zip archive
/// (matches the on-disk names so the archive mirrors the folder layout).
pub fn archive_name(rel_path: &str) -> String {
    rel_path
        .split('/')
        .filter(|s| !s.is_empty())
        .map(sanitize_segment)
        .collect::<Vec<_>>()
        .join("/")
}

/// Replace characters that are invalid in Windows path segments, and defuse
/// reserved device names (CON, NUL, COM1…), which Windows rejects as filenames
/// regardless of extension.
pub(crate) fn sanitize_segment(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
                || (c as u32) < 0x20
            {
                '_'
            } else {
                c
            }
        })
        .collect();
    let trimmed = cleaned.trim_end_matches([' ', '.']);
    if trimmed.is_empty() {
        "_".to_string()
    } else if is_reserved_device_name(trimmed) {
        format!("_{trimmed}")
    } else {
        trimmed.to_string()
    }
}

/// Windows reserves CON/PRN/AUX/NUL/COM1-9/LPT1-9 — also with any extension
/// (`CON.mp4` is just as unusable as `CON`).
fn is_reserved_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("");
    let up = stem.to_ascii_uppercase();
    matches!(up.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (up.len() == 4
            && (up.starts_with("COM") || up.starts_with("LPT"))
            && up.as_bytes()[3].is_ascii_digit()
            && up.as_bytes()[3] != b'0')
}

#[cfg(test)]
mod tests {
    use super::{classify, sanitize_segment, RetryPolicy};
    use crate::Error;

    #[test]
    fn sanitizes_invalid_chars() {
        assert_eq!(sanitize_segment("a/b:c*?"), "a_b_c__");
        assert_eq!(sanitize_segment("trailing. "), "trailing");
        assert_eq!(sanitize_segment("ok name.mp4"), "ok name.mp4");
        // `..` must never survive as a path segment (traversal).
        assert_eq!(sanitize_segment(".."), "_");
    }

    #[test]
    fn sanitizes_reserved_device_names() {
        assert_eq!(sanitize_segment("CON"), "_CON");
        assert_eq!(sanitize_segment("con.mp4"), "_con.mp4");
        assert_eq!(sanitize_segment("COM1"), "_COM1");
        assert_eq!(sanitize_segment("LPT9.txt"), "_LPT9.txt");
        // Not reserved: COM0, or names merely containing/starting with these.
        assert_eq!(sanitize_segment("COM0"), "COM0");
        assert_eq!(sanitize_segment("CONCERT.mp4"), "CONCERT.mp4");
        assert_eq!(sanitize_segment("NULLABLE"), "NULLABLE");
    }

    #[test]
    fn classifies_retry_policy() {
        // RD cold-cache → patient
        assert_eq!(
            classify(&Error::RealDebrid {
                status: 404,
                code: Some(24),
                message: "unavailable_file".into(),
            }),
            RetryPolicy::Patient
        );
        // Rate limit / server error → normal retry
        assert_eq!(
            classify(&Error::RealDebrid {
                status: 429,
                code: None,
                message: "slow down".into(),
            }),
            RetryPolicy::Normal
        );
        // Truncated transfer → normal retry
        assert_eq!(
            classify(&Error::Incomplete {
                got: 1,
                expected: 2
            }),
            RetryPolicy::Normal
        );
        // Unsupported/removed → fatal
        assert_eq!(
            classify(&Error::RealDebrid {
                status: 400,
                code: Some(7),
                message: "unsupported".into(),
            }),
            RetryPolicy::Fatal
        );
    }
}
