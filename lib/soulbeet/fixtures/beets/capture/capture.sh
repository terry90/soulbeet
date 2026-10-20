#!/usr/bin/env bash
# Runs one real beets import the way soulbeet runs it and stores the transcript
# as a classifier fixture in ../<CASE>/.
#
# usage: BEETS_VENV=DIR FIXTURE_WORK=DIR capture.sh CASE CONFIG [extra import args]
#   BEETS_VENV    a beets 2.11.0 venv
#   FIXTURE_WORK  scratch dir for sources, libraries and raw transcripts
#   CONFIG    a file under configs/. A name that is not there is passed to
#             beets as a path that does not exist.
#   The source must already be in $FIXTURE_WORK/CASE/src (cases.sh builds it).
#   SRC=path  imports that path instead (for a missing source).
#   PRIME=1   first imports $FIXTURE_WORK/CASE/prime with the same config, so
#             the captured run meets a duplicate.
#   FAKESRC_MODE, FAKESRC_CRASH are read by fakesrc.py.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
fixtures=$(dirname "$here")
usage="usage: BEETS_VENV=DIR FIXTURE_WORK=DIR $0 CASE CONFIG [extra import args]"
venv=${BEETS_VENV:?$usage}
work=${FIXTURE_WORK:?$usage}

case_name=$1 cfg_name=$2
shift 2
w=$work/$case_name
src=${SRC:-$w/src}
out=$fixtures/$case_name

if [ -f "$here/configs/$cfg_name" ]; then
  cfg=$w/config.yaml
  sed "s|@PLUGINPATH@|$here|" "$here/configs/$cfg_name" >"$cfg"
else
  cfg=$w/$cfg_name
fi

mkdir -p "$w/home" "$w/beetsdir" "$w/music"
beet() {
  env HOME="$w/home" BEETSDIR="$w/beetsdir" NO_COLOR=1 PYTHONUNBUFFERED=1 \
    "$venv/bin/beet" -v -c "$cfg" -l "$w/music/.beets_library.db" -d "$w/music" "$@" </dev/null
}

if [ "${PRIME:-0}" = 1 ]; then
  beet import -q --log "$w/prime.log" "$@" "$w/prime" >"$w/prime.out" 2>&1 ||
    { echo "priming import failed, see $w/prime.out" >&2; exit 1; }
fi

# Counted before the run, which moves the files.
mode=album
for arg in "$@"; do [ "$arg" = -s ] && mode=singleton; done
submitted=0
if [ -e "$src" ]; then
  submitted=$(find "$src" -type f \( -iname '*.flac' -o -iname '*.mp3' -o -iname '*.m4a' \
    -o -iname '*.ogg' -o -iname '*.aac' -o -iname '*.wav' -o -iname '*.wma' -o -iname '*.opus' \) | wc -l)
fi

rm -f "$w/runlog.txt"
rc=0
beet import -q --log "$w/runlog.txt" "$@" "$src" >"$w/stdout.raw" 2>"$w/stderr.raw" || rc=$?
[ -f "$w/runlog.txt" ] || : >"$w/runlog.txt"

mkdir -p "$out"
pybase=$("$venv/bin/python" -c 'import sys; print(sys.base_prefix)')
# Absolute paths become neutral roots, longest first so /work stays last.
python3 -I - "$w" "$out" "$venv" "$pybase" "$here" <<'EOF'
import sys
w, out, venv, pybase, plugins = sys.argv[1:]
roots = [
    (f"{w}/src", "/src"), (f"{w}/prime", "/prime"), (f"{w}/music", "/music"),
    (venv, "/venv"), (pybase, "/python"), (plugins, "/plugins"), (w, "/work"),
]
for raw, name in [("stdout.raw", "stdout.txt"), ("stderr.raw", "stderr.txt"), ("runlog.txt", "runlog.txt")]:
    text = open(f"{w}/{raw}", encoding="utf-8", errors="surrogateescape").read()
    for path, root in roots:
        text = text.replace(path, root)
    open(f"{out}/{name}", "w", encoding="utf-8", errors="surrogateescape").write(text)
EOF
printf '{"exit":%d,"submitted":%d,"mode":"%s"}\n' "$rc" "$submitted" "$mode" >"$out/meta.json"

if grep -q "$HOME" "$out"/*.txt; then
  echo "warning: $case_name still contains $HOME" >&2
fi
echo "$case_name: exit $rc, submitted $submitted, $mode"
