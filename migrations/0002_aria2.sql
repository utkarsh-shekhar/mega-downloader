-- Phase 2: Aria2 backend + remote path mapping.
--
-- The engine can hand Real-Debrid downloads to an Aria2 instance (JSON-RPC) so
-- we get rate-limiting / segmentation / AriaNg visibility. When Aria2 finishes
-- a file it stays in its own download area; the engine then MOVES it into the
-- correct nested MEGA folder using the "remote path mapping" pattern (same idea
-- as Sonarr/Radarr): translate the Aria2-reported path prefix to the local
-- path prefix, then rename/copy.

-- Remote path mappings (Sonarr/Radarr style).
--   remote_path : path prefix as Aria2 sees/reports it (e.g. /rdtdownloads)
--   local_path  : same folder as this engine opens it (e.g. /mnt/media/...)
-- Longest-common-prefix match wins when several mappings exist.
CREATE TABLE IF NOT EXISTS path_mappings (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    remote_path TEXT NOT NULL,
    local_path  TEXT NOT NULL,
    position    INTEGER NOT NULL DEFAULT 0,
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Track which Aria2 download (gid) belongs to a transfer so onDownloadComplete
-- knows where to move the finished file, and which transport served it.
ALTER TABLE transfers ADD COLUMN aria2_gid TEXT;
ALTER TABLE transfers ADD COLUMN source   TEXT NOT NULL DEFAULT 'engine'; -- engine | aria2
