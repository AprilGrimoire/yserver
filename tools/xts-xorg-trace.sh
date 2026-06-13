#!/usr/bin/env bash
# Run xts5 against Xorg through x11trace inside vng. This avoids the
# host X server entirely and gives a real Xorg reference for XI
# protocol tracing.
#
# Usage:
#   tools/xts-xorg-trace.sh <SCENARIO> [TIMEOUT_SECONDS] [SCREEN]
#
# Defaults:
#   SCENARIO = XI
#   TIMEOUT  = 1200
#   SCREEN   = 1600x900
#
# Artifacts:
#   xts-xorg.log    - Xorg stderr / log output
#   xts-xorg.xtrace - x11trace wire capture
set -euo pipefail

SCENARIO=${1:-XI}
TIMEOUT=${2:-1200}
SCREEN=${3:-1600x900}

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)

cd "$repo_root"

for bin in x11trace xdpyinfo; do
    if ! command -v "$bin" >/dev/null 2>&1; then
        echo "error: $bin not installed" >&2
        exit 1
    fi
done

XORG_BIN=${XORG_BIN:-/usr/lib/Xorg}
if [[ ! -x "$XORG_BIN" ]]; then
    echo "error: $XORG_BIN not executable" >&2
    exit 1
fi

XORG_DISPLAY=${XORG_DISPLAY:-:18}
TRACE_DISPLAY=${TRACE_DISPLAY:-:19}
XORG_LOG=${XORG_LOG:-xts-xorg.log}
TRACE_FILE=${TRACE_FILE:-xts-xorg.xtrace}

rm -f "$XORG_LOG" "$TRACE_FILE"

tmpdir=$(mktemp -d)
cleanup() {
    kill -TERM "${xtrace_pid:-}" "${xorg_pid:-}" 2>/dev/null || true
    wait "${xtrace_pid:-}" 2>/dev/null || true
    wait "${xorg_pid:-}" 2>/dev/null || true
    rm -rf "$tmpdir"
}
trap cleanup EXIT

cat >"$tmpdir/xorg.conf" <<EOF
Section "ServerLayout"
    Identifier "Layout0"
    Screen 0 "Screen0" 0 0
EndSection

Section "Device"
    Identifier "Card0"
    Driver "modesetting"
EndSection

Section "Monitor"
    Identifier "Monitor0"
EndSection

Section "Screen"
    Identifier "Screen0"
    Device "Card0"
    Monitor "Monitor0"
    DefaultDepth 24
    SubSection "Display"
        Depth 24
        Modes "$SCREEN"
    EndSubSection
EndSection
EOF

"$XORG_BIN" "$XORG_DISPLAY" \
    -config "$tmpdir/xorg.conf" \
    -ac \
    -noreset \
    -nolisten tcp \
    -logfile "$XORG_LOG" \
    -verbose 3 \
    >"$XORG_LOG" 2>&1 &
xorg_pid=$!

for _ in $(seq 1 100); do
    if DISPLAY="$XORG_DISPLAY" xdpyinfo >/dev/null 2>&1; then
        break
    fi
    if ! kill -0 "$xorg_pid" >/dev/null 2>&1; then
        echo "error: Xorg exited before the socket came up" >&2
        tail -40 "$XORG_LOG" >&2 || true
        exit 2
    fi
    sleep 0.1
done
if ! DISPLAY="$XORG_DISPLAY" xdpyinfo >/dev/null 2>&1; then
    echo "error: Xorg socket /tmp/.X11-unix/X${XORG_DISPLAY#:} never became usable" >&2
    tail -40 "$XORG_LOG" >&2 || true
    exit 2
fi

x11trace -k -d "$XORG_DISPLAY" -D "$TRACE_DISPLAY" -n -o "$TRACE_FILE" &
xtrace_pid=$!

for _ in $(seq 1 100); do
    if DISPLAY="$TRACE_DISPLAY" xdpyinfo >/dev/null 2>&1; then
        break
    fi
    if ! kill -0 "$xtrace_pid" >/dev/null 2>&1; then
        echo "error: x11trace exited before the proxy display came up" >&2
        tail -40 "$XORG_LOG" >&2 || true
        exit 3
    fi
    sleep 0.1
done
if ! DISPLAY="$TRACE_DISPLAY" xdpyinfo >/dev/null 2>&1; then
    echo "error: proxy display $TRACE_DISPLAY never became usable" >&2
    tail -40 "$XORG_LOG" >&2 || true
    exit 3
fi

DISPLAY="$TRACE_DISPLAY" tools/xts-run.sh "$TRACE_DISPLAY" "$SCENARIO" "$TIMEOUT"
