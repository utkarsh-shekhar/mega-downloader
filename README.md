# MEGA Structured Downloader

A Windows desktop app — and a single-process Linux/Docker self-hosted server —
that downloads MEGA folder links with the **folder structure fully preserved**,
using **Real-Debrid** as the download source to avoid MEGA's ~5 GB free-transfer
cap.

Existing tools make you pick one or the other:

- **Real-Debrid** gets past the transfer cap, but flattens every folder into
  one big list of files.
- **MegaBasterd** keeps the folder structure, but is unreliable.

This tool does both: it reads the folder tree directly from MEGA (folder
*listing* is unmetered), fetches the file bytes through Real-Debrid, and
writes every file into its correct nested directory — with pause/resume,
automatic retries, crash-safe resumable downloads, integrity checks, and
optional structure-preserving zip export.

When Real-Debrid can't serve a file, the engine falls back to downloading that
file directly from MEGA so a whole-folder run doesn't fail on a few
stragglers. Fallback downloads do count against MEGA's normal transfer quota.

## Requirements

- **A Real-Debrid premium subscription.** Paste your API token in the app's
  Settings. Without it the tool loses its main advantage, since only the
  native-MEGA fallback would remain.
- Windows 10/11 x64 (packaged desktop app), **or** any Linux/docker host via
  the self-contained build (see ["Run on Linux"](#run-on-linux-with-one-command)
  and ["Deploy with Docker"](#deploy-with-docker-single-self-contained-image)).

## Install

Download the latest installer from the
[Releases page](https://github.com/greatgreatasset/mega-downloader/releases)
and run it.

> **Windows SmartScreen warning:** the installer is **not code-signed**, so
> Windows will show *"Windows protected your PC"* the first time you run it.
> Click **More info → Run anyway**. This is expected for unsigned open-source
> software — if you'd rather not trust a prebuilt binary, build it yourself
> from source (instructions below).

This software is provided as-is, with no warranty or support.

## Usage

1. Open the app and paste your Real-Debrid API token in **Settings**.
2. Paste a MEGA folder link and click **Inspect** to preview the folder tree.
3. Click **Download**. Files land in `<Downloads>/MegaDownloader` by default
   (configurable in Settings), in their original nested folders.

Jobs persist across restarts and can be paused, resumed, or deleted
individually. A finished job can be exported as a zip that preserves the
folder structure.

## How it works

```
UI (React, localhost)  ──REST + WebSocket──►  Engine (Rust, headless)
                                              ├─ mega        folder tree + keys
                                              ├─ realdebrid  byte source
                                              ├─ db          SQLite, restart-safe queue
                                              └─ server      axum REST + WS
```

The engine runs as a separate process (a Tauri sidecar in the packaged app),
so downloads survive a UI crash or reload. State lives in SQLite; interrupted
downloads resume from where they left off.

| Path | What |
|------|------|
| `crates/engine` | Core library: MEGA parsing/crypto, Real-Debrid client, DB |
| `crates/server` | `mega-downloader` binary: axum REST + WebSocket API |
| `migrations`    | SQLite schema |
| `ui`            | Vite + React + Tailwind frontend |
| `src-tauri`     | Tauri desktop shell |

## Building from source

Prerequisites: Rust (MSVC toolchain) and Node.js.

**Run the web version (development):**

```bash
# Terminal 1 — engine on http://127.0.0.1:8787
cargo run -p server

# Terminal 2 — UI on http://localhost:5173
cd ui && npm install && npm run dev
```

**Build the desktop installer:**

```bash
npm install                 # root: installs the Tauri CLI

# 1) build the engine and stage it as the Tauri sidecar
cargo build -p server --release
cp target/release/mega-downloader.exe \
   src-tauri/binaries/mega-downloader-x86_64-pc-windows-msvc.exe

# 2) bundle → installer under src-tauri/target/release/bundle/nsis/
npm run tauri build
```

The headless engine can also be used on its own: run
`cargo run -p server` and drive it over its REST + WebSocket API
(state in `./mega-downloader.db`, downloads to `./downloads`; override with
`DB_PATH` / `DOWNLOAD_DIR`).

## Run on Linux with one command

Prerequisites: **Rust** and **Node.js**. Then, from the repo root:

```bash
./run.sh                 # debug build, then serve UI + API on http://127.0.0.1:8787
```

The script builds the engine and the React UI, then runs a **single
self-contained process** that serves both the frontend and the REST/WebSocket
API on one port, so the whole app is reachable at one URL.

```bash
./run.sh --release        # optimized build
./run.sh --detach         # run in the background (log: /tmp/mega-downloader.log)
./run.sh --stop           # stop a background run
./run.sh --docker         # build & run the Docker image instead
BIND_ADDR=0.0.0.0:8787 ./run.sh   # expose on your LAN / VM
```

Useful environment variables: `BIND_ADDR` (default `127.0.0.1:8787`),
`DB_PATH` (default `./mega-downloader.db`), `DOWNLOAD_DIR` (default
`./downloads`), `MEGA_UI_DIR` (compiled frontend dir; defaults to `ui/dist`).

## Deploy with Docker (single self-contained image)

A multi-stage `Dockerfile` produces one image containing **both** the compiled
React UI and the Rust engine/server. `mega-downloader` serves real-debrid
downloads, the job queue API, live WebSocket progress, *and* the web UI — so
deployment is just one container/one service, on Docker or Kubernetes.

```bash
# Build & run
docker build -t mega-downloader .
docker run --rm -p 8787:8787 \
  -v mega-downloader-data:/data \
  -e BIND_ADDR=0.0.0.0:8787 \
  mega-downloader
# open http://localhost:8787
```

Or with compose:

```bash
docker compose up -d --build
```

Inside the image the database and downloaded files are written to **`/data`**
(mount a volume there). The image listens on `0.0.0.0:8787`; the `EXPOSE` and
`PORT` are fixed at `8787`. The full set of runtime env vars is identical to
the Linux run (`BIND_ADDR`, `DB_PATH=/data/mega-downloader.db`,
`DOWNLOAD_DIR=/data/downloads`).

### Kubernetes

The same image deploys as a plain `Deployment` + `Service` + `PVC`:

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: mega-downloader
spec:
  replicas: 1
  selector:
    matchLabels: { app: mega-downloader }
  template:
    metadata:
      labels: { app: mega-downloader }
    spec:
      containers:
        - name: mega-downloader
          image: mega-downloader:latest
          ports: [{ containerPort: 8787 }]
          env:
            - { name: BIND_ADDR, value: "0.0.0.0:8787" }
            - { name: DB_PATH, value: "/data/mega-downloader.db" }
            - { name: DOWNLOAD_DIR, value: "/data/downloads" }
          volumeMounts:
            - { name: data, mountPath: /data }
          resources:
            requests: { cpu: "250m", memory: "256Mi" }
            limits:   { cpu: "2",     memory: "1Gi" }
      volumes:
        - name: data
          persistentVolumeClaim:
            claimName: mega-downloader-data
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: mega-downloader-data
spec:
  accessModes: [ReadWriteOnce]
  resources:
    requests:
      storage: 50Gi   # adjust to your download library size
---
apiVersion: v1
kind: Service
metadata:
  name: mega-downloader
spec:
  selector: { app: mega-downloader }
  ports:
    - { port: 8787, targetPort: 8787 }
```

> **Scale to 1 replica only** — jobs/state live in the local SQLite
> database, so more than one re-cluster would leave job state split across
> pods. Keep `replicas: 1` (or use a shared PVC).

## License

[MIT](LICENSE)
