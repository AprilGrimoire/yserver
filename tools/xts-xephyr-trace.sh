#!/usr/bin/env bash
# Run xts5 against Xephyr through x11trace, so we can capture an
# Xorg-family reference trace for XI debugging without the vng/KMS
# layer. Use this to compare wire-level behavior against yserver.
#
# Usage:
#   tools/xts-xephyr-trace.sh <SCENARIO> [TIMEOUT_SECONDS] [SCREEN]
#
# Defaults:
#   SCENARIO = XI
#   TIMEOUT  = 1200
#   SCREEN   = 1600x900
#
# Artifacts:
#   xts-xephyr.log    - Xephyr stderr
#   xts-xephyr.xtrace - x11trace wire capture
set -euo pipefail

SCENARIO=${1:-XI}
TIMEOUT=${2:-1200}
SCREEN=${3:-1600x900}

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)

cd "$repo_root"

if [[ -z "${DISPLAY:-}" ]]; then
    echo "error: need a host DISPLAY (run from a graphical session)" >&2
    exit 1
fi

for bin in Xephyr x11trace; do
    if ! command -v "$bin" >/dev/null 2>&1; then
        echo "error: $bin not installed" >&2
        exit 1
    fi
done

XEPHYR_DISPLAY="${XEPHYR_DISPLAY:-:18}"
TRACE_DISPLAY="${TRACE_DISPLAY:-:19}"
XEPHYR_LOG="${XEPHYR_LOG:-xts-xephyr.log}"
TRACE_FILE="${TRACE_FILE:-xts-xephyr.xtrace}"

rm -f "$XEPHYR_LOG" "$TRACE_FILE"

Xephyr -screen "$SCREEN" -title "xts-xephyr" "$XEPHYR_DISPLAY" \
    >"$XEPHYR_LOG" 2>&1 &
xephyr_pid=$!

cleanup() {
    kill -TERM "$xtrace_pid" "$xephyr_pid" 2>/dev/null || true
    wait "$xtrace_pid" 2>/dev/null || true
    wait "$xephyr_pid" 2>/dev/null || true
}
trap cleanup EXIT

for _ in $(seq 1 50); do
    [[ -S "/tmp/.X11-unix/X${XEPHYR_DISPLAY#:}" ]] && break
    sleep 0.1
done
if [[ ! -S "/tmp/.X11-unix/X${XEPHYR_DISPLAY#:}" ]]; then
    echo "error: Xephyr socket /tmp/.X11-unix/X${XEPHYR_DISPLAY#:} never appeared" >&2
    tail -20 "$XEPHYR_LOG" >&2 || true
    exit 2
fi

x11trace -d "$XEPHYR_DISPLAY" -D "$TRACE_DISPLAY" -n -o "$TRACE_FILE" &
xtrace_pid=$!

for _ in $(seq 1 50); do
    [[ -S "/tmp/.X11-unix/X${TRACE_DISPLAY#:}" ]] && break
    sleep 0.1
done
if [[ ! -S "/tmp/.X11-unix/X${TRACE_DISPLAY#:}" ]]; then
    echo "error: x11trace socket /tmp/.X11-unix/X${TRACE_DISPLAY#:} never appeared" >&2
    tail -20 "$XEPHYR_LOG" >&2 || true
    exit 3
fi

DISPLAY="$TRACE_DISPLAY" tools/xts-run.sh "$TRACE_DISPLAY" "$SCENARIO" "$TIMEOUT"
