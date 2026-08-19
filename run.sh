#!/usr/bin/env bash
#
# mega-downloader — single-command run on Linux.
#
# Builds the Rust engine/server and the React UI, then runs ONE self-contained
# process that serves both the frontend and the REST/WebSocket API on a single
# port (default 8787). Open http://127.0.0.1:8787 to use it.
#
#   ./run.sh            # debug build
#   ./run.sh --release  # optimized build
#   ./run.sh --docker   # build & run the Docker image instead
#   ./run.sh --stop     # stop a background run
#
# Env you can override when running:
#   BIND_ADDR        host:port to listen on (default 127.0.0.1:8787;
#                     use 0.0.0.0:8787 to expose on your LAN / in Docker)
#   DB_PATH          SQLite path (default ./mega-downloader.db)
#   DOWNLOAD_DIR     where files land (default ./downloads)
#
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIND_ADDR="${BIND_ADDR:-127.0.0.1:8787}"
PROFILE="debug"
MODE="native"
DETACH=0

for arg in "$@"; do
  case "$arg" in
    --release) PROFILE="release" ;;
    --docker)  MODE="docker" ;;
    --stop)    MODE="stop" ;;
    -d|--detach) DETACH=1 ;;
    -h|--help)
      grep '^#' "$0" | sed '1d' | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "Unknown option: $arg (try --help)" >&2
      exit 1
      ;;
  esac
done

cd "$ROOT"

build_ui() {
  echo "── Building React UI ──"
  ( cd ui && npm install && npm run build )
}

case "$MODE" in
  stop)
    echo "── Stopping mega-downloader (if running) ──"
    stopped=0
    if [[ -f /tmp/mega-downloader.pid ]]; then
      kill "$(cat /tmp/mega-downloader.pid)" 2>/dev/null && stopped=1
      rm -f /tmp/mega-downloader.pid
    fi
    # Catch engine processes by their binary path (target/... or /app/...).
    # IMPORTANT: do not `pkill -f mega-downloader` broadly — that can match the
    # calling shell's command line (which includes this repo's path).
    if pkill -f '/(target/(debug|release)|app)/mega-downloader' 2>/dev/null; then
      stopped=1
    fi
    if [[ "$stopped" == "1" ]]; then
      echo "Stopped."
    elif ss -tlnp 2>/dev/null | grep -q ':8787 '; then
      echo "Unable to stop the listener on :8787 automatically — stop it manually."
    else
      echo "Not running."
    fi
    exit 0
    ;;

  docker)
    if ! command -v docker >/dev/null 2>&1; then
      echo "Docker not installed." >&2; exit 1
    fi
    echo "── Building Docker image ──"
    docker build -t mega-downloader "$ROOT"
    # Map host ${BIND_ADDR%:*} : 8787 -> container 8787. Default host bind is
    # 127.0.0.1 so the engine isn't exposed on the LAN unless the user wants it.
    host_bind="${BIND_ADDR%:*}"
    echo "── Running container on ${host_bind}:8787 ──"
    docker run --rm \
      -p "${host_bind}:8787:8787" \
      -v mega-downloader-data:/data \
      mega-downloader
    exit 0
    ;;
esac

# --- native mode -------------------------------------------------------------
for tool in cargo node npm; do
  command -v "$tool" >/dev/null 2>&1 || { echo "Missing prerequisite: $tool (see README)" >&2; exit 1; }
done

echo "── Rust version: $(cargo --version) / Node: $(node --version) ──"

build_ui

echo "── Building engine/server ($PROFILE) ──"
if [[ "$PROFILE" == "release" ]]; then
  cargo build --release -p server
  BIN="$ROOT/target/release/mega-downloader"
else
  cargo build -p server
  BIN="$ROOT/target/debug/mega-downloader"
fi

# Serve the compiled UI from this same process.
export MEGA_UI_DIR="${MEGA_UI_DIR:-$ROOT/ui/dist}"
export BIND_ADDR DB_PATH DOWNLOAD_DIR

echo ""
echo "── Ready ─────────────────────────────────────────────────"
display_addr="${BIND_ADDR}"
if [[ "$display_addr" == "0.0.0.0"* || "$display_addr" == "::"* ]]; then
  display_addr="127.0.0.1:${BIND_ADDR##*:}"
fi
echo "   Listen   :  ${BIND_ADDR}"
echo "   Open     :  http://${display_addr}"
echo "   DB       :  ${DB_PATH:-./mega-downloader.db}"
echo "   Downloads:  ${DOWNLOAD_DIR:-./downloads}"
echo "──────────────────────────────────────────────────────────"
echo "Press Ctrl-C to stop."

if [[ "$DETACH" == "1" ]]; then
  nohup "$BIN" >/tmp/mega-downloader.log 2>&1 &
  echo $! > /tmp/mega-downloader.pid
  sleep 1
  echo "Started in background (pid $(cat /tmp/mega-downloader.pid), log: /tmp/mega-downloader.log)."
else
  exec "$BIN"
fi
