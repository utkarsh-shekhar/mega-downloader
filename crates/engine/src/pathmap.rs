//! Remote path mapping (the Sonarr/Radarr pattern).
//!
//! Aria2 and this engine may see the same media through different path
//! prefixes. E.g. inside Aria2's container a file is at `/rdtdownloads/foo.mkv`,
//! but on the host/this engine the same file is `/mnt/media/media/rdtdownloads/foo.mkv`.
//!
//! A `PathMapping` maps a `remote_path` prefix (as Aria2 reports it) to a
//! `local_path` prefix (as this engine opens it). When multiple mappings exist,
//! the one with the **longest matching prefix** wins, so a specific rule beats a
//! broad one.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One remote→local path mapping row (mirrors the `path_mappings` DB table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathMapping {
    pub id: Option<i64>,
    pub remote_path: String,
    pub local_path: String,
    pub position: i64,
}

impl PathMapping {
    pub fn new(remote_path: impl Into<String>, local_path: impl Into<String>) -> Self {
        Self {
            id: None,
            remote_path: remote_path.into(),
            local_path: local_path.into(),
            position: 0,
        }
    }
}

/// Translate an Aria2-reported path to the local path using the best mapping.
///
/// Returns `None` if no mapping applies (caller should treat the path as
/// already local).
pub fn resolve_mapping<'a>(mappings: &'a [PathMapping], remote_path: &str) -> Option<PathBuf> {
    let remote = Path::new(remote_path);

    // Longest matching prefix wins.
    let mut best: Option<(&PathMapping, usize)> = None;
    for m in mappings {
        let prefix = Path::new(&m.remote_path);
        // A path "starts with" the prefix if prefix is an ancestor (or equal).
        if remote.strip_prefix(prefix).is_ok() {
            let len = m.remote_path.len();
            if best.map(|(_, bl)| len > bl).unwrap_or(true) {
                best = Some((m, len));
            }
        }
    }

    let (mapping, _) = best?;
    let rest = remote.strip_prefix(Path::new(&mapping.remote_path)).ok()?;
    let mut local = PathBuf::from(&mapping.local_path);
    local.push(rest);
    Some(local)
}

/// Normalize a user-provided path (trailing slash trimmed, forward-slash style)
/// so prefix comparisons behave predictably.
pub fn normalize(p: &str) -> String {
    let mut s = p.trim().trim_end_matches('/').to_string();
    // Collapse duplicate leading slashes beyond a single root "/".
    if s.len() > 1 && s.starts_with("//") {
        s = format!("/{}", s.trim_start_matches('/'));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(remote: &str, local: &str) -> PathMapping {
        PathMapping::new(remote, local)
    }

    #[test]
    fn matches_exact_prefix() {
        let mappings = vec![map("/rdtdownloads", "/mnt/media/media/rdtdownloads")];
        let out = resolve_mapping(&mappings, "/rdtdownloads/job1/video.mkv").unwrap();
        assert_eq!(
            out,
            PathBuf::from("/mnt/media/media/rdtdownloads/job1/video.mkv")
        );
    }

    #[test]
    fn longest_prefix_wins() {
        let mappings = vec![
            map("/rdtdownloads", "/mnt/media/media/rdtdownloads"),
            map("/rdtdownloads/job1", "/mnt/media/mega-downloader/job1"),
        ];
        let out = resolve_mapping(&mappings, "/rdtdownloads/job1/video.mkv").unwrap();
        assert_eq!(out, PathBuf::from("/mnt/media/mega-downloader/job1/video.mkv"));
    }

    #[test]
    fn no_match_returns_none() {
        let mappings = vec![map("/rdtdownloads", "/mnt/media/media/rdtdownloads")];
        assert!(resolve_mapping(&mappings, "/elsewhere/file.mkv").is_none());
    }

    #[test]
    fn direct_local_path_returns_none() {
        // If Aria2 already reports a local path we won't map it again.
        let mappings = vec![map("/rdtdownloads", "/mnt/media/media/rdtdownloads")];
        assert!(resolve_mapping(&mappings, "/mnt/media/somewhere/file.mkv").is_none());
    }
}
