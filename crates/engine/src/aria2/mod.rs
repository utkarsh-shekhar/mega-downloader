//! Minimal Aria2 JSON-RPC client.
//!
//! The engine can hand Real-Debrid downloads to a local/external Aria2
//! instance so we get rate-limiting, segmentation, and AriaNg visibility for
//! free. This module is a thin, async JSON-RPC client over HTTP POST:
//!
//! - `get_version()`   — health/connectivity probe (`aria2.getVersion`)
//! - `add_uri()`       — queue a URL with per-download options
//! - `tell_status()`   — poll a gid (completion, path, error)
//! - `remove()`        — drop an active/paused download
//!
//! Aria2 also supports WebSocket push notifications, but a poll loop driven by
//! the engine's own job queue is simpler and keeps state in SQLite (the source
//! of truth), so we poll rather than subscribe. The default RPC endpoint is
//! `http://127.0.0.1:6800/jsonrpc`; a secret, if set, is sent as `token:…`.

use std::time::Duration;

use reqwest::Client;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};

use crate::Result;

/// A single Aria2 file download.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Aria2File {
    /// Absolute path to the downloaded file once complete.
    #[serde(default)]
    pub path: String,
    /// Total length in bytes.
    #[serde(default)]
    pub length: String,
    /// Bytes already downloaded.
    #[serde(default)]
    pub completed_length: String,
    /// true once the file is fully downloaded.
    #[serde(default)]
    pub selected: String,
}

/// `aria2.tellStatus` response (the fields we care about).
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct AriaStatus {
    pub gid: String,
    /// active | waiting | paused | error | complete | removed
    #[serde(default)]
    pub status: String,
    /// Total length (bytes) as a decimal string.
    #[serde(default)]
    pub total_length: String,
    /// Bytes downloaded so far.
    #[serde(default)]
    pub completed_length: String,
    /// Files this download produces.
    #[serde(default)]
    pub files: Vec<Aria2File>,
    #[serde(default)]
    pub error_code: Option<String>,
    /// Human-readable error message (base64 encoded by Aria2).
    #[serde(default)]
    pub error_message: Option<String>,
    /// Directory the file lives in (set at addUri time).
    #[serde(default)]
    pub dir: String,
}

/// Options we set per download when queueing via Aria2.
#[derive(Debug, Clone, Default, Serialize)]
pub struct AddUriOptions {
    /// Download directory for this file.
    pub dir: String,
    /// Per-file max speed (aria2 format: "0" = unlimited, e.g. "5M").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_download_limit: Option<String>,
    /// Number of connections per server (1 = single connection, safer for RD).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_connection_per_server: Option<String>,
    /// Keep the downloader restart-safe: continue partial files on restart.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continue_field: Option<String>,
    /// A human label shown in AriaNg for this transfer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// Connection config for the Aria2 RPC endpoint.
#[derive(Debug, Clone, Default)]
pub struct AriaConfig {
    /// e.g. `http://127.0.0.1:6800/jsonrpc`. Empty/`none` disables Aria2.
    pub rpc_url: Option<String>,
    /// RPC secret; sent as `token:<secret>`.
    pub secret: Option<String>,
    /// Default per-download speed limit (aria2 format). None = unlimited.
    pub max_download_limit: Option<String>,
}

impl AriaConfig {
    /// True when an RPC URL is configured (Aria2 backend enabled).
    pub fn enabled(&self) -> bool {
        matches!(&self.rpc_url, Some(u) if !u.trim().is_empty())
    }
}

/// Async JSON-RPC client for Aria2 over HTTP POST.
#[derive(Debug, Clone)]
pub struct Aria2Client {
    url: String,
    secret: Option<String>,
    http: Client,
}

impl Aria2Client {
    /// Build a client from a config. Returns `None` when Aria2 is disabled.
    pub fn from_config(cfg: &AriaConfig) -> Option<Self> {
        let url = cfg.rpc_url.as_deref()?.trim();
        if url.is_empty() {
            return None;
        }
        Some(Self {
            url: url.to_string(),
            secret: cfg.secret.clone(),
            http: Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .ok()?,
        })
    }

    /// `aria2.getVersion` — connectivity + version probe.
    pub async fn get_version(&self) -> Result<String> {
        let v: Value = self.call("aria2.getVersion", json!([])).await?;
        Ok(v.get("version")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string())
    }

    /// Queue a download. Returns the assigned `gid`.
    pub async fn add_uri(&self, uris: &[String], options: AddUriOptions) -> Result<String> {
        // Aria2 wants options as a JSON object; serialize ours.
        let opts = serde_json::to_value(options)?;
        let v: Value = self
            .call("aria2.addUri", json!([uris, opts]))
            .await?;
        v.as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| crate::Error::Other("aria2.addUri returned no gid".into()))
    }

    /// `aria2.tellStatus` for a gid.
    pub async fn tell_status(&self, gid: &str) -> Result<AriaStatus> {
        self.call::<AriaStatus>("aria2.tellStatus", json!([gid])).await
    }

    /// `aria2.remove` a download (works for active/waiting; also pauses).
    pub async fn remove(&self, gid: &str) -> Result<()> {
        let _: String = self.call("aria2.remove", json!([gid])).await?;
        Ok(())
    }

    /// `aria2.purgeDownloadResult` — clear finished results (keeps the queue tidy).
    #[allow(dead_code)]
    pub async fn purge(&self, gids: &[String]) -> Result<()> {
        let _: Value = self.call("aria2.purgeDownloadResult", json!([])).await?;
        let _ = gids;
        Ok(())
    }

    /// Generic JSON-RPC call. Adds the `token:` parameter first when a secret
    /// is configured. Returns the `result` field, or `Err` on an Aria2 error.
    async fn call<T: DeserializeOwned>(&self, method: &str, mut params: Value) -> Result<T> {
        // Insert token as the first positional param if a secret is set.
        if let Some(secret) = &self.secret {
            let arr = params.as_array_mut().expect("params must be array");
            arr.insert(0, json!(format!("token:{secret}")));
        }
        let body = json!({
            "jsonrpc": "2.0",
            "id": "mega-dl",
            "method": method,
            "params": params,
        });
        let resp = self
            .http
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .map_err(crate::Error::Http)?;
        let v: Value = resp.json().await.map_err(crate::Error::Http)?;

        if let Some(err) = v.get("error") {
            let code = err.get("code").and_then(Value::as_i64).unwrap_or(-1);
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("aria2 rpc error");
            return Err(crate::Error::Aria2 {
                method: method.to_string(),
                code,
                message: msg.to_string(),
            });
        }
        let result = v
            .get("result")
            .ok_or_else(|| crate::Error::Other("aria2 rpc: missing result".into()))?
            .clone();
        serde_json::from_value(result).map_err(crate::Error::Json)
    }
}
