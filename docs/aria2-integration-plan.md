# Aria2 Integration + Remote Path Mapping — Implementation Plan

**Status:** Plan (awaiting approval) · **Author:** Ixora for Utkarsh · **Date:** 2026-08-19

## 1. Goal

Make the MEGA Structured Downloader download through the **existing homelab Aria2** service
(running on the Mini PC docker host) instead of its built-in Rust streamer, so we get:

- **Rate limiting / max download speed** (per-download + overall)
- **AriaNg visibility** (transfers appear in the existing AriaNg web UI)
- **Resume, retries, segmented connections** — Aria2 handles the byte transfer
- **Folder-structure preservation** still works via **remote path mapping** + a move-on-complete
  step (the *arr stack pattern)

The engine keeps doing what it's uniquely good at (the "structure brain"): parse MEGA links,
reconstruct the decrypted folder tree, unrestrict Real-Debrid links, and maintain the
restart-safe SQLite job queue. Aria2 becomes the actual data mover.

## 2. Architecture

```
UI (React)                                                  Mini PC docker host (192.168.10.160)
   │                                                              │
   │ REST + WS (8787)                                             │
   ▼                                                              │
mega-downloader engine ──────────── JSON-RPC/6800 ──────────────► aria2 (AriaNg :8080)
  ├─ mega        : folder tree + keys                            │    downloads to
  ├─ realdebrid  : unrestrict → plaintext URL                     │    /rdtdownloads
  ├─ db (SQLite) : jobs, nodes, transfers, SETTINGS (path maps)   │         │
  ├─ server      : axum REST + WS + static UI                     │         ▼ host path
  └─ aria2 client: addUri + onDownloadComplete                    │   /mnt/media/media/rdtdownloads
        │                                                          │         │ (NFS from OMV)
        └─── on complete: apply path mapping ─────────► rename/copy file ──► /mnt/media/<target nested folder>
```

**Key property (verified 2026-08-19):** `/mnt/media/media/rdtdownloads` (aria2 host path) and the
engine's target dir are on the **same NFS mount** (`192.168.10.50:/export/homelab`, OMV). So the
completed file can be moved with an **instant local `rename()`** — no data crosses the network.

## 3. Download paths

| Path | Source of bytes | Who downloads | Encrypted? | Notes |
|------|-----------------|---------------|------------|-------|
| **Real-Debrid** (main) | Aria2 fetches RD's plaintext URL | **Aria2** | No (RD decrypted) | Full rate-limit + AriaNg |
| **MEGA fallback** | AES-128-CTR encrypted | Rust engine | Yes | Aria2 *cannot* decrypt MEGA → keep existing streamer |

A **per-file fallback**: if Aria2 is unreachable (or a link expired), the engine falls back to its
built-in streamer for that file, then tries Aria2 again later — a whole-job run never dies on a
transient Aria2 outage.

## 4. Remote Path Mapping (the *arr pattern)

Both sides see the *same* files, just with different path prefixes:

- **Remote path** — as **Aria2** reports the file (e.g. `/rdtdownloads/ab3456/SomeFile.mkv`)
- **Local path** — how **this engine/VM** opens the same file (e.g. `/mnt/media/mega-downloader/ab3456/SomeFile.mkv`)

> In the homelab deployment both resolve to the same NFS export, so the move is a local rename.
> If a mapping's `localPath` is on a *different* filesystem, the engine falls back to **copy+remove**
> (slower but correct). A mapping is matched by **longest-common-prefix** so the most specific rule wins.

**Defaults (single mapping, UI-editable, DB-persisted):**

| remotePath | localPath |
|------------|-----------|
| `/rdtdownloads` | `/mnt/media/media/rdtdownloads` |

## 5. Move-on-Complete

On `aria2.onDownloadComplete` (gid) →
1. `aria2.tellStatus(gid)` → completed file `path` + `dir`
2. Look up the job that owns this gid (engine tracks `gid → transfer_id/job`)
3. Resolve the file's **target MEGA folder path** from the job's saved tree
4. Apply the **path mapping** to translate `aria2 path → local path`
5. `rename()` (same-fs) or `copy+remove` (cross-fs) the file to `target/<file>`
6. Mark transfer `done`, update job progress, emit WS event
7. If a Real-Debrid link **expired** (download failed with a link-error), re-unrestrict and
   re-`addUri` with a fresh URL

## 6. Config surface

**Settings UI additions (all saved to SQLite via existing settings mechanism):**

| Setting | Type | Example | Notes |
|---------|------|---------|-------|
| Aria2 RPC URL | text | `http://192.168.10.160:6800/jsonrpc` | empty = disable Aria2 |
| Aria2 RPC secret | password | `secret` | `token:<secret>` in JSON-RPC |
| Max download speed | text (RFC/aria2) | `50M` | passed as `max-download-limit` per download; blank = unlimited |
| Remote path mappings | list of `{remotePath, localPath}` | table UI (add/remove rows) | like Sonarr/Radarr |

**Env vars (for the homelab compose, override DB defaults):** `ARIA2_RPC_URL`, `ARIA2_SECRET`,
`MAX_DOWNLOAD_SPEED`, `DOWNLOAD_DIR`.

Aria2 client is **off by default** (RPC URL empty) — existing users and tests keep the built-in
streamer until Aria2 is configured.

## 7. DB schema (new migration `0002_aria2.sql`)

```sql
-- Aria2/engine settings are kept in the existing key/value `settings` table:
--   aria2_rpc_url, aria2_rpc_secret, max_download_speed

-- Remote path mappings (Sonarr/Radarr "remote path mapping" pattern)
CREATE TABLE path_mappings (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    remote_path TEXT NOT NULL,
    local_path  TEXT NOT NULL,
    position    INTEGER NOT NULL DEFAULT 0,
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Track which Aria2 gid belongs to which job transfer, so onDownloadComplete
-- knows where to move the file.
ALTER TABLE transfers ADD COLUMN aria2_gid TEXT;
ALTER TABLE transfers ADD COLUMN source TEXT NOT NULL DEFAULT 'engine';  -- engine | aria2
```

## 8. REST API additions

| Method | Path | Purpose |
|--------|------|---------|
| GET | `/api/path-mappings` | list mappings |
| POST | `/api/path-mappings` | add mapping |
| PUT | `/api/path-mappings/{id}` | update mapping |
| DELETE | `/api/path-mappings/{id}` | remove mapping |

Existing settings GET/POST (`/api/settings`) extended with `aria2_rpc_url` (value, not secret),
`aria2_rpc_secret_set` (bool), `max_download_speed`.

## 9. UI additions (`ui/src/App.tsx`, Settings section)

New "Download source" block:
- Aria2 RPC URL + secret + max speed inputs (Save via existing settings call)
- "Remote path mappings" sub-table: rows `remotePath → localPath`, add/remove buttons
- Small status line: "Aria2 connected: engine v0.1.0 · aria2 1.36.0" (from a new `/api/aria2/status`
  endpoint that calls `aria2.getVersion`)

## 10. Homelab service (minipc_3e)

New folder `minipc_3e/mega-downloader/` with:

**`docker-compose.yaml`** (following existing conventions — config on SSD, media via NFS, log cap):

```yaml
services:
  mega-downloader:
    build: { context: /srv/mega-downloader, dockerfile: Dockerfile }   # or image from registry
    image: mega-downloader:latest
    container_name: mega-downloader
    restart: unless-stopped
    ports:
      - "8787:8787"          # UI + API (self-contained single process)
    environment:
      - BIND_ADDR=0.0.0.0:8787
      - ARIA2_RPC_URL=http://aria2:6800/jsonrpc   # docker network, not host IP
      - ARIA2_SECRET=secret
      - MAX_DOWNLOAD_SPEED=50M
      - DB_PATH=/data/mega-downloader.db
      - DOWNLOAD_DIR=/mnt/media/mega-downloader     # final nested-folder target on 16TB NFS
    volumes:
      - ./config:/data            # DB + settings on SSD
      - /mnt/media/mega-downloader:/mnt/media/mega-downloader   # 16TB NFS (target)
      - /mnt/media/media/rdtdownloads:/rdtdownloads              # read the aria2 area (same NFS)
    depends_on:
      - aria2
    healthcheck:
      test: ["CMD", "wget", "-qO-", "http://localhost:8787/api/health"]   # or curl
      interval: 30s
      timeout: 10s
      retries: 3
    logging: { driver: json-file, options: { max-size: "10m", max-file: "3" } }
```

**Key mounts** (all on the same OMV NFS -> fast renames):
- `./config:/data` — SQLite DB (aria2 RPC config, path mappings) on **SSD**
- `/mnt/media/media/rdtdownloads:/rdtdownloads` — read aria2's output (shared with aria2)
- `/mnt/media/mega-downloader:/mnt/media/mega-downloader` — final download target on 16TB

> Aria2 and mega-downloader sharing NFS is why renames are instant: both containers mount
> `/mnt/media` (OMV export) read-write. The engine renames from `/rdtdownloads/...` to
> `/mnt/media/mega-downloader/...` directly on host `/mnt/media/...` — one NFS rename syscall.

Register in `minipc_3e/init.sh` "Done" summary + `README.md` service table.

**Detect like other services:** init.sh auto-discovers `*/docker-compose.yaml`. If compose can't
`build` (no local source), commit built image or push `mega-downloader:latest` to a local registry;
simplest: point `build.context` at a checkout of the repo on the VM, or use an image tag.

## 11. Testing (Docker VM, minipc `192.168.10.160`)

All testing happens in the docker VM where the real Aria2 + NFS live.

1. **Aria2 connectivity:** unit-check `aria2.getVersion` against the live RPC (already CA: reachable,
   secret `secret`, v1.36.0).
2. **DB + migrations:** `0002_aria2.sql` apply cleanly on a fresh DB; path_mappings CRUD via REST.
3. **UI:** settings block renders, save/load path mappings from DB, Aria2 status line shows connected.
4. **End-to-end (real download):** use a real (small) MEGA folder link + a Real-Debrid token →
   observe: aria2 gets `addUri`, file bytes appear in `/rdtdownloads/<job>/`, on complete the file
   **moves** to `/mnt/media/mega-downloader/<job>/<nested path>`, `aria2.tellStatus` shows complete,
   WS event fires, job marked done.
5. **Rate limit:** set `MAX_DOWNLOAD_SPEED=1M`, confirm Aria2 enforces the cap (visible in AriaNg).
6. **Fallback:** stop aria2 container → engine falls back to built-in streamer for RD files, resumes
   correctly.
7. **Expired link:** simulate RD link expiry → engine re-unrestricts and re-adds to aria2.
8. **Compose validation:** `docker compose -f megadownloader/docker-compose.yaml config --quiet`.

## 12. Deliverables / repo changes

**mega-downloader repo:**
- `migrations/0002_aria2.sql`
- `crates/engine/src/aria2/` — JSON-RPC client (addUri, tellStatus, getVersion, notifications)
- `crates/engine/src/aria2/mod.rs`, `client.rs`, `move.rs` (path mapping + rename/copy logic)
- `crates/engine/src/download.rs` — route RD files through Aria2 when configured; fallback
- `crates/engine/src/db.rs` — settings + path_mappings helpers
- `crates/server/src/routes.rs` — `/api/path-mappings*`, `/api/aria2/status`, settings fields
- `ui/src/App.tsx` — "Download source" + path-mapping UI
- `ui/src/api.ts` — new fetch helpers
- `README.md` — Aria2 + remote path mapping docs
- `Dockerfile` — ensure `wget`/`curl` for healthcheck (or use /api/health via wget in base image)

**homelab repo:**
- `minipc_3e/mega-downloader/docker-compose.yaml`
- `minipc_3e/mega-downloader/README.md`
- `minipc_3e/MEGA_DOWNLOADER.md` or section in `minipc_3e/README.md` service table
- `minipc_3e/init.sh` — echo line in "Done" summary

## 13. Risks / decisions

- **Aria2 can't decrypt MEGA** → MEGA-fallback files stay in Rust streamer. Acceptable (rare).
- **Short-lived RD links** → re-unrestrict on expiry. Must map `gid→job` even if the initial addUri
  silently fails.
- **Concurrency vs RD limits** → keep Aria2 per-file `--split=1` (or 2) for RD URLs; the engine sets
  this for RD downloads, higher split only for pure-http (non-RD) sources if any.
- **Docker swarm/K8s single-replica** → still SQLite-local; keep replicas=1 (unchanged).
- **NFS rename atomicity** → rename on NFSv3 across the same server export is generally fine; if it
  ever returns EXDEV/link errors, fall back to copy+remove (already planned).

## 14. Open decisions for Utkarsh

1. **Image build for homelab** — build inside the VM from a repo checkout, or push `mega-downloader:latest`
   to a local registry (e.g. on the VM / a registry container)?
2. **Aria2 RPC on docker network** — connect via `http://aria2:6800` (compose network, cleanest) or the
   host IP `192.168.10.160:6800` (works regardless of DNS)? Recommend `http://aria2:6800`.
3. **Max speed default** — set a sane default (e.g. `0` = unlimited, or `50M`)? Recommend `0` (unlimited)
   with the UI to set it, to avoid surprising slow-downs.
4. **Whether Mega-downloader should appear in AriaNg** with a distinguishable label — set Aria2
   `title`/`comment` on addUri so its transfers are recognizable.
