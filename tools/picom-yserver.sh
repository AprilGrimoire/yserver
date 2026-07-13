#!/usr/bin/env bash
# Run picom against yserver as a RENDER + COMPOSITE smoke harness.
#
# picom v13's `xrender` backend issues a wider RENDER surface
# (kernel blur via XRenderSetPictureFilter "convolution", per-window
# alpha via XRenderComposite, Damage-driven redraws) than typical
# WM compositors (xfwm4, marco) do. Useful for shaking out RENDER
# / COMPOSITE coverage gaps once the rendering model re-enables
# compositor support.
#
# NB (2026-05-15): on the current rendering model picom redraws
# once and then goes silent — Damage / XFixes / COW infrastructure
# is incomplete, so picom never gets notified of subsequent paint.
# This harness is parked until the rendering rewrite addresses that.
#
# Optional: pass an xclient program to run alongside (default:
# xclock) so there's a visible window for picom to redirect:
#
#   tools/picom-yserver.sh xterm
#
# Stops yserver + picom on Ctrl+C.

set -u

NEST_DISPLAY=":7"
PICOM_LOG="./picom.log"
WM_LOG="./wm-picom.log"
CLIENT="${1:-xclock}"

if ! command -v picom >/dev/null; then
    echo "picom not installed (pacman -S picom)" >&2
    exit 1
fi

# Build yserver.
cargo build --bin yserver

# Spawn yserver.
RUST_LOG="${RUST_LOG:-debug}" RUST_BACKTRACE=1 \
    target/debug/yserver >yserver-hw.log 2>&1 &
YSERVER_PID=$!

# Temp picom config — kernel blur with a 9x9 gaussian (a real-world
# size). `backend = "xrender"` forces the RENDER path (the GLX
# backend uses OpenGL and bypasses XRenderSetPictureFilter).
PICOM_CONF=$(mktemp /tmp/picom-yserver.XXXXXX.conf)

cleanup() {
    rm -f "$PICOM_CONF"
    if [[ -n "${PICOM_PID:-}" ]]; then
        kill -TERM "$PICOM_PID" 2>/dev/null || true
        wait "$PICOM_PID" 2>/dev/null || true
    fi
    if [[ -n "${WM_PID:-}" ]]; then
        kill -TERM "$WM_PID" 2>/dev/null || true
        wait "$WM_PID" 2>/dev/null || true
    fi
    if [[ -n "${CLIENT_PID:-}" ]]; then
        kill -TERM "$CLIENT_PID" 2>/dev/null || true
        wait "$CLIENT_PID" 2>/dev/null || true
    fi
    kill -TERM "$YSERVER_PID" 2>/dev/null || true
    wait "$YSERVER_PID" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

cat >"$PICOM_CONF" <<'EOF'
# RENDER convolution smoke config (phase 2 of
# render-convolution-filter plan). `xrender` backend + kernel
# blur-method ⇒ picom calls XRenderSetPictureFilter("convolution",
# <weights>) when it actually renders a blur — which only happens
# when a window is semi-transparent (so the area behind it is
# visible and needs blurring). With everything opaque, picom
# redirects but never emits the filter.
#
# `inactive-opacity = 0.7` + `focus-exclude = "!a"` (= "not active")
# forces every window to be painted at 0.7 opacity, which makes
# blur-background fire on every paint.
backend = "xrender";
blur-method = "kernel";
blur-kern = "9x9gaussian";
blur-background = true;
blur-background-exclude = [];
inactive-opacity = 0.7;
inactive-opacity-override = true;
focus-exclude = "!a";
EOF

sleep 2

# Need a window manager so client windows actually get mapped /
# stacked. Try twm (small), then matchbox, then no WM — picom still
# starts but no windows will render until something maps a top-
# level.
WM_BIN=""
for candidate in fvwm3 wmaker e16 fvwm twm matchbox-window-manager; do
    if command -v "$candidate" >/dev/null; then
        WM_BIN="$candidate"
        break
    fi
done

if [[ -n "$WM_BIN" ]]; then
    echo "starting WM: $WM_BIN"
    env -u WAYLAND_DISPLAY DISPLAY="$NEST_DISPLAY" "$WM_BIN" >"$WM_LOG" 2>&1 &
    WM_PID=$!
    sleep 1
else
    echo "no WM found (tried twm, matchbox-window-manager, fvwm3, fvwm) — picom" \
         "still starts but mapped clients may be invisible" >&2
fi

echo "starting client: $CLIENT"
env -u WAYLAND_DISPLAY DISPLAY="$NEST_DISPLAY" "$CLIENT" >/dev/null 2>&1 &
CLIENT_PID=$!
sleep 1

echo "starting picom (config $PICOM_CONF, log $PICOM_LOG)"
env -u WAYLAND_DISPLAY DISPLAY="$NEST_DISPLAY" \
    picom --config "$PICOM_CONF" --log-level debug 2>&1 | tee "$PICOM_LOG" &
PICOM_PID=$!

echo
echo "yserver:  yserver-hw.log"
echo "picom:    $PICOM_LOG"
echo "wm:       $WM_LOG"
echo "client:   $CLIENT (DISPLAY=$NEST_DISPLAY)"
echo
echo "Ctrl+C to stop."
wait "$PICOM_PID"
