#!/usr/bin/env bash
# One-command local seeded debug stack for Soulbeet Web (Desktop & Mobile viewports).
#
# Starts:
#   1. Mock Navidrome (OpenSubsonic + Native API, SVG covers, playable ambient WAV) on :14533
#   2. Optional e2e service stubs on dedicated ports :15030..15070 (Deezer, MB, slskd, Archive, LB)
#   3. Real Soulbeet debug binary on 0.0.0.0:9766 (or 127.0.0.1 with --local-only) with SOULBEET_DEV_AUTOLOGIN=1
#   4. Seeds isolated SQLite DB under .runtime-dev/ (never touches prod or real music)
#
# Usage:
#   ./scripts/dev-seeded.sh [up] [--build] [--all-mixes] [--local-only] [--port 9766]
#   ./scripts/dev-seeded.sh down
#   ./scripts/dev-seeded.sh reset [--all-mixes] [--local-only]
#   ./scripts/dev-seeded.sh status
#   ./scripts/dev-seeded.sh logs

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(dirname "$SCRIPT_DIR")"
SERVER_DIR="$REPO_DIR/target/dx/web/debug/web"
RUNTIME="$REPO_DIR/.runtime-dev"
PID_DIR="$RUNTIME/pids"
LOG_DIR="$RUNTIME/logs"
DB_PATH="$RUNTIME/data/soulbeet-dev.db"
MUSIC_ROOT="$RUNTIME/music"
DOWNLOADS_DIR="$RUNTIME/downloads"

PORT="${DEV_PORT:-9766}"
NAVIDROME_PORT="${NAVIDROME_PORT:-14533}"
# Dedicated high ports for e2e stubs so we never collide with a real local slskd/service
STUB_SLSKD_PORT="${DEV_SLSKD_PORT:-15030}"
STUB_MB_PORT="${DEV_MB_PORT:-15050}"
STUB_LB_PORT="${DEV_LB_PORT:-15060}"
STUB_LABS_PORT="${DEV_LABS_PORT:-15061}"
STUB_DEEZER_PORT="${DEV_DEEZER_PORT:-15062}"
STUB_GDSTUDIO_PORT="${DEV_GDSTUDIO_PORT:-15063}"
STUB_ARCHIVE_PORT="${DEV_ARCHIVE_PORT:-15064}"
STUB_AIRDCPP_PORT="${DEV_AIRDCPP_PORT:-15065}"
STUB_OIDC_PORT="${DEV_OIDC_PORT:-15070}"

SECRET_KEY="${DEV_SECRET_KEY:-soulbeet-dev-seeded-secret-key-0123456789abcdef}"
BIND_HOST="0.0.0.0"
FORCE_BUILD=0
ALL_MIXES=0
CMD="up"

validate_port() {
  local name="$1" val="$2"
  if [[ ! "$val" =~ ^[0-9]+$ ]] || (( val < 1 || val > 65535 )); then
    echo "ERROR: invalid port for $name: '$val' (expected integer 1..65535)" >&2
    exit 1
  fi
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    up|down|reset|status|logs)
      CMD="$1"
      shift
      ;;
    --build)
      FORCE_BUILD=1
      shift
      ;;
    --all-mixes)
      ALL_MIXES=1
      shift
      ;;
    --local-only)
      BIND_HOST="127.0.0.1"
      shift
      ;;
    --port)
      if [[ $# -lt 2 ]]; then
        echo "ERROR: --port requires a port number" >&2
        exit 1
      fi
      PORT="$2"
      shift 2
      ;;
    -h|--help)
      sed -n '2,17p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "Unknown option: $1" >&2
      exit 1
      ;;
  esac
done

validate_port "PORT" "$PORT"
validate_port "NAVIDROME_PORT" "$NAVIDROME_PORT"

if (( ${#SECRET_KEY} < 32 )); then
  echo "ERROR: DEV_SECRET_KEY must be at least 32 characters" >&2
  exit 1
fi

detect_lan_ip() {
  local ip
  ip="$(ip -4 route get 1.1.1.1 2>/dev/null | awk '{for(i=1;i<=NF;i++) if($i=="src") {print $(i+1); exit}}' || true)"
  echo "${ip:-127.0.0.1}"
}

# Terminates a daemon launched with `launch_daemon` by signaling its entire process group,
# so wrapper trees like `npx -> tsx -> node` never leave orphan children behind.
stop_pid() {
  local name="$1"
  local pidfile="$PID_DIR/$name.pid"
  if [[ -f "$pidfile" ]]; then
    local pid
    pid="$(cat "$pidfile")"
    if [[ "$pid" =~ ^[0-9]+$ ]]; then
      kill -- -"$pid" 2>/dev/null || kill "$pid" 2>/dev/null || true
      for _ in $(seq 1 15); do
        kill -0 -- -"$pid" 2>/dev/null || kill -0 "$pid" 2>/dev/null || break
        sleep 0.2
      done
      kill -9 -- -"$pid" 2>/dev/null || kill -9 "$pid" 2>/dev/null || true
    fi
    rm -f "$pidfile"
  fi
}

# Launches a command in its own process group/session and records the exact session-leader PGID.
launch_daemon() {
  local name="$1" logfile="$2"
  shift 2
  local pidfile="$PID_DIR/$name.pid"
  rm -f "$pidfile"
  setsid bash -c 'echo $$ > "$1"; shift; exec "$@"' _ "$pidfile" "$@" >"$logfile" 2>&1 </dev/null &
  for _ in $(seq 1 30); do
    [[ -s "$pidfile" ]] && break
    sleep 0.05
  done
}

# Fails fast if a required port is already occupied by an unrelated process.
assert_port_free() {
  local port="$1" label="$2"
  if command -v ss >/dev/null 2>&1; then
    if ss -tln "( sport = :$port )" 2>/dev/null | grep -q LISTEN; then
      echo "ERROR: port $port ($label) is already in use by another process. Stop it or pick another port." >&2
      ss -tlnp "( sport = :$port )" 2>/dev/null || true
      exit 1
    fi
  fi
}

wait_for() {
  local url="$1" name="$2" tries=0
  until curl -fso /dev/null --max-time 2 "$url"; do
    tries=$((tries + 1))
    if [[ $tries -gt 60 ]]; then
      echo "ERROR: $name did not come up at $url" >&2
      echo "Check logs in $LOG_DIR" >&2
      exit 1
    fi
    sleep 0.3
  done
}

stop_all_daemons() {
  stop_pid soulbeet
  stop_pid mock-navidrome
  stop_pid stubs
}

cmd_down() {
  stop_all_daemons
  echo "Seeded dev stack stopped."
}

cmd_status() {
  local lan_ip
  lan_ip="$(detect_lan_ip)"
  for name in mock-navidrome stubs soulbeet; do
    if [[ -f "$PID_DIR/$name.pid" ]] && kill -0 "$(cat "$PID_DIR/$name.pid")" 2>/dev/null; then
      echo "  [running] $name (pid $(cat "$PID_DIR/$name.pid"))"
    else
      echo "  [stopped] $name"
    fi
  done
  echo "  Local:          http://127.0.0.1:${PORT}"
  echo "  LAN:            http://${lan_ip}:${PORT}"
  echo "  Mock Navidrome: http://${lan_ip}:${NAVIDROME_PORT}"
}

cmd_logs() {
  tail -n 60 -f "$LOG_DIR"/*.log
}

start_soulbeet() {
  local nav_public_url="$1"
  shift
  assert_port_free "$PORT" "soulbeet"
  # Scrub ambient prod/OIDC/beets env vars so caller shell settings never interfere
  (cd "$SERVER_DIR" && \
    launch_daemon soulbeet "$LOG_DIR/soulbeet.log" \
      env -u PUBLIC_URL -u OIDC_ISSUER_URL -u OIDC_CLIENT_ID -u OIDC_CLIENT_SECRET -u OIDC_PROVIDER_NAME -u BEETS_CONFIG \
        IP="$BIND_HOST" \
        PORT="$PORT" \
        DATABASE_URL="sqlite:${DB_PATH}" \
        SECRET_KEY="$SECRET_KEY" \
        DOWNLOAD_PATH="$DOWNLOADS_DIR" \
        REGISTRATION="open" \
        NAVIDROME_URL="http://127.0.0.1:${NAVIDROME_PORT}" \
        NAVIDROME_PUBLIC_URL="$nav_public_url" \
        NAVIDROME_MUSIC_PATH="$MUSIC_ROOT" \
        SOULBEET_DEV_AUTOLOGIN="1" \
        RUST_LOG="${RUST_LOG:-info,sqlx=warn}" \
        "$@" \
        ./server)
  wait_for "http://127.0.0.1:${PORT}/healthz" "soulbeet"
}

cmd_up() {
  if [[ "$FORCE_BUILD" -eq 1 || ! -x "$SERVER_DIR/server" ]]; then
    if [[ -d "$REPO_DIR/node_modules/@tailwindcss" ]]; then
      echo "Rebuilding Tailwind CSS (web/assets/tailwind.css)..."
      (cd "$REPO_DIR" && npx @tailwindcss/cli -i ./web/assets/input.css -o ./web/assets/tailwind.css)
    fi
    echo "Building debug web bundle (dx bundle --package web)..."
    (cd "$REPO_DIR" && dx bundle --package web)
  fi

  stop_all_daemons
  mkdir -p "$PID_DIR" "$LOG_DIR" "$(dirname "$DB_PATH")" "$MUSIC_ROOT/library" "$MUSIC_ROOT/shared" "$DOWNLOADS_DIR"

  # Clean up any started daemons if startup or seeding aborts mid-way
  trap 'echo "Startup failed; stopping dev stack..." >&2; stop_all_daemons' ERR INT

  local lan_ip
  lan_ip="$(detect_lan_ip)"
  local nav_public_url
  if [[ "$BIND_HOST" == "127.0.0.1" ]]; then
    nav_public_url="${NAVIDROME_PUBLIC_URL:-http://127.0.0.1:${NAVIDROME_PORT}}"
  else
    nav_public_url="${NAVIDROME_PUBLIC_URL:-http://${lan_ip}:${NAVIDROME_PORT}}"
  fi

  # 1. Start mock Navidrome (OpenSubsonic + Native API + playable WAV + SVG covers)
  assert_port_free "$NAVIDROME_PORT" "mock-navidrome"
  launch_daemon mock-navidrome "$LOG_DIR/mock-navidrome.log" \
    env NAVIDROME_PORT="$NAVIDROME_PORT" \
        NAVIDROME_HOST="$BIND_HOST" \
        NAVIDROME_MUSIC_PATH="$MUSIC_ROOT" \
        node "$SCRIPT_DIR/dev-seeded/mock-navidrome.mjs"
  wait_for "http://127.0.0.1:${NAVIDROME_PORT}/ping" "mock-navidrome"

  # 2. Start e2e service stubs on dedicated high ports if e2e dependencies are installed
  local stub_env=()
  local dev_slskd_url=""
  if [[ -d "$REPO_DIR/e2e/node_modules" ]]; then
    if [[ ! -d "$REPO_DIR/e2e/.fixtures/audio" ]]; then
      echo "Generating e2e audio fixtures..."
      (cd "$REPO_DIR/e2e" && npm run --silent fixtures)
    fi
    assert_port_free "$STUB_SLSKD_PORT" "slskd-stub"
    assert_port_free "$STUB_MB_PORT" "mb-stub"
    assert_port_free "$STUB_DEEZER_PORT" "deezer-stub"
    (cd "$REPO_DIR/e2e" && \
      launch_daemon stubs "$LOG_DIR/stubs.log" \
        env SLSKD_PORT="$STUB_SLSKD_PORT" \
            MB_PORT="$STUB_MB_PORT" \
            LB_PORT="$STUB_LB_PORT" \
            LABS_PORT="$STUB_LABS_PORT" \
            DEEZER_PORT="$STUB_DEEZER_PORT" \
            GDSTUDIO_PORT="$STUB_GDSTUDIO_PORT" \
            ARCHIVE_PORT="$STUB_ARCHIVE_PORT" \
            AIRDCPP_PORT="$STUB_AIRDCPP_PORT" \
            OIDC_PORT="$STUB_OIDC_PORT" \
            DOWNLOADS_DIR="$DOWNLOADS_DIR" \
            npx tsx stubs/src/main.ts)
    wait_for "http://127.0.0.1:${STUB_MB_PORT}/ws/2/recording?query=ping" "e2e-stubs"
    dev_slskd_url="http://127.0.0.1:${STUB_SLSKD_PORT}"
    stub_env=(
      "MUSICBRAINZ_HOST=127.0.0.1:${STUB_MB_PORT}"
      "LISTENBRAINZ_HOST=127.0.0.1:${STUB_LB_PORT}"
      "LISTENBRAINZ_LABS_HOST=127.0.0.1:${STUB_LABS_PORT}"
      "DEEZER_HOST=127.0.0.1:${STUB_DEEZER_PORT}"
      "GDSTUDIO_HOST=127.0.0.1:${STUB_GDSTUDIO_PORT}"
      "ARCHIVE_HOST=127.0.0.1:${STUB_ARCHIVE_PORT}"
    )
  else
    echo "Note: e2e/node_modules not found; running without local Deezer/MB/slskd stubs."
  fi

  # 3. Start Soulbeet debug server with SOULBEET_DEV_AUTOLOGIN=1
  start_soulbeet "$nav_public_url" ${stub_env[@]+"${stub_env[@]}"}

  # 4. Seed SQLite DB & verify config
  local seed_args=()
  if [[ "$ALL_MIXES" -eq 1 ]]; then
    seed_args+=(--all-mixes)
  fi
  DEV_DB_PATH="$DB_PATH" \
  DEV_BASE_URL="http://127.0.0.1:${PORT}" \
  DEV_SLSKD_URL="$dev_slskd_url" \
  SECRET_KEY="$SECRET_KEY" \
  NAVIDROME_MUSIC_PATH="$MUSIC_ROOT" \
    node "$SCRIPT_DIR/dev-seeded/seed.mjs" ${seed_args[@]+"${seed_args[@]}"}

  # Restart Soulbeet once after seeding so services (slskd/app_config) load the seeded config cleanly
  stop_pid soulbeet
  start_soulbeet "$nav_public_url" ${stub_env[@]+"${stub_env[@]}"}

  trap - ERR INT

  echo ""
  echo "Soulbeet seeded dev stack is ready (auto-login enabled as admin):"
  echo "  Local:          http://127.0.0.1:${PORT}"
  if [[ "$BIND_HOST" == "0.0.0.0" ]]; then
    echo "  LAN (phone):    http://${lan_ip}:${PORT}"
    echo "  WARNING:        Bound to 0.0.0.0 with passwordless admin auto-login (trusted LAN only; use --local-only for 127.0.0.1)."
  fi
  echo "  Mock Navidrome: ${nav_public_url}"
  echo "  Logs:           ${LOG_DIR}"
  echo "  Stop:           ./scripts/dev-seeded.sh down"
}

case "$CMD" in
  up) cmd_up ;;
  down) cmd_down ;;
  reset)
    cmd_down
    rm -rf "$RUNTIME"
    cmd_up
    ;;
  status) cmd_status ;;
  logs) cmd_logs ;;
esac
