#!/bin/bash
# Quick vng MATE smoke: brings yserver up with full mate-session, drives
# a brief synthetic drag via xdotool, captures telemetry.
# Diagnostic — not a perf gate.
set -u
cd /home/jos/Projects/yserver

mkdir -p /tmp/.X11-unix
rm -f yserver-vng.log mate-vng.log yserver-vng.submit.tsv

xdg_rd=$(mktemp -d -t yserver-vng.XXXXXX)
chmod 700 "$xdg_rd"

echo "=== STARTING yserver ==="
YSERVER_LOOP_TELEMETRY=1 \
    YSERVER_SUBMIT_TRACE=yserver-vng.submit.tsv \
    MESA_LOADER_DRIVER_OVERRIDE=zink \
    XDG_RUNTIME_DIR="$xdg_rd" \
    RUST_LOG=info RUST_BACKTRACE=1 \
    target/release/yserver > yserver-vng.log 2>&1 &
ys_pid=$!

for i in $(seq 1 30); do [ -S /tmp/.X11-unix/X7 ] && break; sleep 1; done
[ -S /tmp/.X11-unix/X7 ] || { echo "FAIL: X socket :7"; tail -30 yserver-vng.log; kill -9 $ys_pid 2>/dev/null; exit 1; }
echo "X socket up after ${i}s"

echo "=== STARTING mate-session ==="
env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET \
    DISPLAY=:7 GDK_BACKEND=x11 XDG_SESSION_TYPE=x11 \
    XDG_RUNTIME_DIR="$xdg_rd" \
    dbus-run-session mate-session --display :7 > mate-vng.log 2>&1 &
ms_pid=$!

sleep 12

echo "=== SYNTHETIC DRAG (15s of windowmove against any xterm if present) ==="
DISPLAY=:7 xterm -geometry 80x20+200+200 -e 'cat' < /dev/null > /dev/null 2>&1 &
xt_pid=$!
sleep 2
xterm_win=$(DISPLAY=:7 xdotool search --name "xterm" 2>/dev/null | head -1 || true)
echo "xterm window id: ${xterm_win:-(none)}"
end=$(($(date +%s) + 15))
i=0
while [ $(date +%s) -lt $end ]; do
    x=$((200 + (i % 10) * 50))
    y=$((200 + ((i / 10) % 5) * 30))
    if [ -n "$xterm_win" ]; then
        DISPLAY=:7 xdotool windowmove "$xterm_win" "$x" "$y" 2>/dev/null || true
    fi
    i=$((i + 1))
done
echo "Drag done: $i ops"

echo "=== SHUTTING DOWN ==="
kill -TERM $xt_pid $ms_pid 2>/dev/null
sleep 3
kill -TERM $ys_pid 2>/dev/null
wait $ys_pid 2>/dev/null
rm -rf "$xdg_rd"

echo "=== PEAK TELEMETRY ==="
for m in "paint_submits/s" "composite_submits/s" "queue_submit2/s" "cow_batches_flushed/s" "cow_copies_coalesced/s" "render_batches_flushed/s" "render_composites_coalesced/s" "cpu_fence_wait_ns/s" "cpu_fence_wait_count/s" "frame_present_count/s"; do
  peak=$(grep "render_telemetry" yserver-vng.log 2>/dev/null | grep -oE "${m}=[0-9]+" | sort -t= -k2 -rn | head -1)
  echo "  $peak"
done
echo
echo "=== ERRORS / PANICS ==="
grep -iE "panic|fatal|abort|ERROR_DEVICE|TIMEOUT" yserver-vng.log 2>/dev/null | head -5
echo "=== END ==="
echo "Full log: yserver-vng.log ($(wc -l < yserver-vng.log) lines)"
