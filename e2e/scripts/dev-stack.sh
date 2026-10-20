#!/usr/bin/env bash
# Runs the e2e stack natively (no docker): the stub services plus two
# soulbeet servers from the binary produced by `dx bundle --package web
# --release`, the main one on 9765 and the issue #90 one on 9766. Useful on
# machines without a container runtime; CI uses compose.e2e.yml.
#
#   scripts/dev-stack.sh up      start stubs + servers, wait until ready
#   scripts/dev-stack.sh down    stop them
#
# Requires Python 3.11 or newer (the beets config check runs `python -P`).
# beets is installed into .runtime/beets-venv, pinned like the image
# (build/requirements-beets.txt). PYTHON picks the interpreter for a new venv.

set -euo pipefail

E2E_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REPO_DIR="$(dirname "$E2E_DIR")"
SERVER_DIR="$REPO_DIR/target/dx/web/release/web"
RUNTIME="$E2E_DIR/.runtime"
PID_DIR="$RUNTIME/pids"
LOG_DIR="$RUNTIME/logs"

cmd="${1:-up}"

stop_pid() {
  local name="$1"
  if [[ -f "$PID_DIR/$name.pid" ]]; then
    kill "$(cat "$PID_DIR/$name.pid")" 2>/dev/null || true
    rm -f "$PID_DIR/$name.pid"
  fi
}

# Kill whatever still listens on a stack port; pid files alone leak orphans
# when `up` runs twice.
free_port() {
  local port="$1"
  local pid
  pid="$(ss -tlnp 2>/dev/null | grep ":$port " | grep -oP 'pid=\K[0-9]+' | head -1 || true)"
  if [[ -n "$pid" ]]; then
    kill "$pid" 2>/dev/null || true
    sleep 0.5
  fi
}

down() {
  stop_pid soulbeet
  stop_pid soulbeet-user90
  stop_pid stubs
  free_port 9765
  free_port 9766
  free_port 5030
  free_port 5050
  echo "stack stopped"
}

wait_for() {
  local url="$1" name="$2" tries=0
  until curl -fso /dev/null --max-time 2 "$url"; do
    tries=$((tries + 1))
    if [[ $tries -gt 60 ]]; then
      echo "$name did not come up ($url)" >&2
      exit 1
    fi
    sleep 0.5
  done
  echo "$name ready"
}

up() {
  if [[ ! -x "$SERVER_DIR/server" ]]; then
    echo "server binary missing; run 'dx bundle --package web --release' in $REPO_DIR first" >&2
    exit 1
  fi

  free_port 9765
  free_port 9766
  free_port 5030
  free_port 5050

  mkdir -p "$PID_DIR" "$LOG_DIR"
  (cd "$E2E_DIR" && npm run --silent fixtures)

  local venv="$RUNTIME/beets-venv"
  if [[ ! -x "$venv/bin/python" ]]; then
    "${PYTHON:-python3}" -m venv "$venv"
  fi
  "$venv/bin/python" -c 'import sys; sys.exit(sys.version_info < (3, 11))' || {
    echo "beets needs Python 3.11 or newer; remove $venv and set PYTHON to one" >&2
    exit 1
  }
  "$venv/bin/pip" install --quiet -c "$REPO_DIR/build/requirements-beets.txt" \
    beets requests musicbrainzngs
  export PATH="$venv/bin:$PATH"
  # The servers' beets config check runs this interpreter.
  export BEETS_PYTHON="$venv/bin/python"

  (cd "$E2E_DIR" && setsid npx tsx stubs/src/main.ts >"$LOG_DIR/stubs.log" 2>&1 </dev/null &
   echo $! >"$PID_DIR/stubs.pid")
  wait_for "http://127.0.0.1:5050/ws/2/recording?query=ping" "stubs"

  (cd "$SERVER_DIR" && \
    DATABASE_URL="sqlite:$RUNTIME/data/soulbeet.db" \
    SECRET_KEY="e2e-secret-key" \
    DOWNLOAD_PATH="$RUNTIME/downloads" \
    BEETS_CONFIG="$E2E_DIR/.fixtures/beets-e2e.yaml" \
    MUSICBRAINZ_HOST="127.0.0.1:5050" \
    PORT=9765 IP=127.0.0.1 \
    setsid ./server >"$LOG_DIR/soulbeet.log" 2>&1 </dev/null &
   echo $! >"$PID_DIR/soulbeet.pid")
  wait_for "http://127.0.0.1:9765/" "soulbeet"

  (cd "$SERVER_DIR" && \
    DATABASE_URL="sqlite:$RUNTIME/data/user90.db" \
    SECRET_KEY="e2e-secret-key" \
    DOWNLOAD_PATH="$RUNTIME/downloads" \
    BEETS_CONFIG="$RUNTIME/user90-config/config.yaml" \
    MUSICBRAINZ_HOST="127.0.0.1:5050" \
    PORT=9766 IP=127.0.0.1 \
    setsid ./server >"$LOG_DIR/soulbeet-user90.log" 2>&1 </dev/null &
   echo $! >"$PID_DIR/soulbeet-user90.pid")
  wait_for "http://127.0.0.1:9766/" "soulbeet-user90"

  echo "stack up: app http://127.0.0.1:9765, issue #90 app http://127.0.0.1:9766,"
  echo "slskd stub :5030, mb stub :5050"
  echo "logs: $LOG_DIR"
}

case "$cmd" in
  up) up ;;
  down) down ;;
  *)
    echo "usage: $0 {up|down}" >&2
    exit 1
    ;;
esac
