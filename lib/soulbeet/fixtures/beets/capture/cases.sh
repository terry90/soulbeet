#!/usr/bin/env bash
# Rebuilds the classifier fixtures from real beets 2.11 runs.
# Needs ffmpeg and a beets 2.11.0 venv (BEETS_VENV). Generated audio and work
# dirs live under FIXTURE_WORK and are not committed.
#
# usage: BEETS_VENV=DIR FIXTURE_WORK=DIR cases.sh [CASE...]   (default: every case)
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
usage="usage: BEETS_VENV=DIR FIXTURE_WORK=DIR $0 [CASE...]"
: "${BEETS_VENV:?$usage}"
work=${FIXTURE_WORK:?$usage}
capture=$here/capture.sh

# mkalbum DIR N [ARTIST] [ALBUM]: N tagged two-second mp3 files.
mkalbum() {
  local dir=$1 n=$2 artist=${3:-Zzqx Artist} album=${4:-Qzz Album} i
  mkdir -p "$dir"
  for i in $(seq 1 "$n"); do
    ffmpeg -loglevel error -y -f lavfi -i "sine=frequency=$((300 + i * 50)):duration=2" \
      -ac 1 -ar 22050 -b:a 32k -metadata artist="$artist" -metadata album_artist="$artist" \
      -metadata album="$album" -metadata title="Song Number $i" -metadata track="$i/$n" \
      "$dir/0$i - Song Number $i.mp3"
  done
}

fresh() {
  if [ -d "$work/$1" ]; then chmod -R u+w "$work/$1"; fi
  rm -rf "${work:?}/$1"
  mkdir -p "$work/$1/src"
  w=$work/$1
}

run_case() {
  case $1 in
    album_strong) fresh "$1"; mkalbum "$w/src" 2; FAKESRC_MODE=strong "$capture" "$1" base.yaml ;;
    single_strong) fresh "$1"; mkalbum "$w/src" 2; FAKESRC_MODE=strong "$capture" "$1" base.yaml -s ;;
    album_weak) fresh "$1"; mkalbum "$w/src" 2; FAKESRC_MODE=weak "$capture" "$1" base.yaml ;;
    single_weak) fresh "$1"; mkalbum "$w/src" 2; FAKESRC_MODE=weak "$capture" "$1" base.yaml -s ;;
    album_none) fresh "$1"; mkalbum "$w/src" 2; FAKESRC_MODE=none "$capture" "$1" base.yaml ;;
    single_none) fresh "$1"; mkalbum "$w/src" 2; FAKESRC_MODE=none "$capture" "$1" base.yaml -s ;;
    dup_ask_album | dup_ask_single | dup_skip | dup_merge | dup_remove | dup_keep)
      local cfg args=()
      case $1 in
        dup_ask_album) cfg=dup_ask.yaml ;;
        dup_ask_single) cfg=dup_ask.yaml args=(-s) ;;
        dup_remove) cfg=base.yaml ;;
        *) cfg=$1.yaml ;;
      esac
      fresh "$1"; mkalbum "$w/prime" 2; mkalbum "$w/src" 2
      PRIME=1 FAKESRC_MODE=strong "$capture" "$1" "$cfg" "${args[@]}"
      ;;
    asis_weak) fresh "$1"; mkalbum "$w/src" 2; FAKESRC_MODE=weak "$capture" "$1" asis.yaml ;;
    asis_dup)
      fresh "$1"; mkalbum "$w/prime" 2; mkalbum "$w/src" 2
      PRIME=1 FAKESRC_MODE=weak "$capture" "$1" asis_dup.yaml
      ;;
    noautotag) fresh "$1"; mkalbum "$w/src" 2; FAKESRC_MODE=strong "$capture" "$1" noautotag.yaml ;;
    # Import as-is: -A runs no lookup, which a failing source would show.
    asis) fresh "$1"; mkalbum "$w/src" 2; FAKESRC_MODE=error "$capture" "$1" base.yaml -A ;;
    asis_single) fresh "$1"; mkalbum "$w/src" 2; FAKESRC_MODE=error "$capture" "$1" base.yaml -s -A ;;
    # A picked release with fewer tracks than the folder: beets leaves the
    # unmatched file behind.
    pick_extra)
      fresh "$1"; mkalbum "$w/src" 3
      CHOICE="pick FakeSrc fake-a-w2" FAKESRC_MODE=weak "$capture" "$1" base.yaml
      ;;
    empty) fresh "$1"; FAKESRC_MODE=strong "$capture" "$1" base.yaml ;;
    nonmusic)
      fresh "$1"; echo "liner notes" >"$w/src/notes.txt"; head -c 2048 /dev/zero >"$w/src/cover.jpg"
      FAKESRC_MODE=strong "$capture" "$1" base.yaml
      ;;
    corrupt_one)
      fresh "$1"; mkalbum "$w/src" 2
      head -c 4096 /dev/zero | tr '\0' x >"$w/src/03 - Song Number 3.mp3"
      FAKESRC_MODE=strong "$capture" "$1" base.yaml
      ;;
    mbdown) fresh "$1"; mkalbum "$w/src" 2; "$capture" "$1" mbdown.yaml ;;
    srcerr) fresh "$1"; mkalbum "$w/src" 2; FAKESRC_MODE=error "$capture" "$1" base.yaml ;;
    missing_path) fresh "$1"; SRC=$w/src/missing FAKESRC_MODE=strong "$capture" "$1" base.yaml ;;
    timid) fresh "$1"; mkalbum "$w/src" 2; FAKESRC_MODE=strong "$capture" "$1" timid.yaml ;;
    missing_config) fresh "$1"; mkalbum "$w/src" 2; FAKESRC_MODE=strong "$capture" "$1" missing.yaml ;;
    readonly_target)
      fresh "$1"; mkalbum "$w/src" 2
      mkdir -p "$w/music/Zzqx Artist"; chmod 555 "$w/music/Zzqx Artist"
      FAKESRC_MODE=strong "$capture" "$1" base.yaml
      chmod 755 "$w/music/Zzqx Artist"
      ;;
    pipeline_crash)
      fresh "$1"; mkalbum "$w/src" 2
      FAKESRC_CRASH=1 FAKESRC_MODE=strong "$capture" "$1" bad_plugin.yaml
      ;;
    bad_plugin) fresh "$1"; mkalbum "$w/src" 2; FAKESRC_MODE=strong "$capture" "$1" bad_plugin.yaml ;;
    multi_task)
      fresh "$1"; mkalbum "$w/src/a1" 2; mkalbum "$w/src/a2" 2 "Weakband Artist" "Other Album"
      FAKESRC_MODE=strong "$capture" "$1" base.yaml
      ;;
    *) echo "unknown case $1" >&2; return 1 ;;
  esac
}

all=(album_strong single_strong album_weak single_weak album_none single_none
  dup_ask_album dup_ask_single dup_skip dup_merge dup_remove dup_keep
  asis_weak asis_dup noautotag asis asis_single pick_extra empty nonmusic corrupt_one mbdown srcerr
  missing_path timid missing_config readonly_target pipeline_crash bad_plugin multi_task)

[ $# -gt 0 ] || set -- "${all[@]}"
for c in "$@"; do run_case "$c"; done
