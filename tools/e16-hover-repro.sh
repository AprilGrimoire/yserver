#!/usr/bin/env bash
# e16 menu-hover regression repro inside vng (run as the vng guest
# command). Brings up yserver + e16, opens the desktop menu via
# xdotool (XTEST), hovers items, and SIGUSR1-dumps the scanout
# before/after hover so the composed frames can be diffed on the host.
set -uo pipefail
cd /home/jos/Projects/yserver
rm -f yserver-scanout-*.ppm yserver-drawable-*.ppm yserver-drawable-*.txt

RUST_LOG="${YSERVER_VNG_RUST_LOG:-debug}" RUST_BACKTRACE=1 \
    target/debug/yserver > yserver-vng-e16.log 2>&1 &
pid=$!
for _ in $(seq 1 150); do
    DISPLAY=:7 xdpyinfo >/dev/null 2>&1 && break
    sleep 0.2
done
DISPLAY=:7 xdpyinfo >/dev/null 2>&1 || { echo "yserver did not come up"; exit 2; }

DISPLAY=:7 e16 > e16-vng.log 2>&1 &
sleep 6

export DISPLAY=:7
# right-click empty desktop → e16 menu opens at the pointer
xdotool mousemove 400 300
sleep 1
xdotool click 3
sleep 2
echo "=== scanout dump 1: menu open, pre-hover ==="
kill -USR1 $pid; sleep 3

# hover down through the items (menu opens with first item at pointer)
for y in 320 340 360 380; do
    xdotool mousemove 440 $y
    sleep 1
done
echo "=== scanout dump 2: post-hover, pointer still on an item ==="
kill -USR1 $pid; sleep 3

# move off the items but keep menu open (pointer to menu title area)
xdotool mousemove 440 290
sleep 1
echo "=== scanout dump 3: pointer off items, menu still open ==="
kill -USR1 $pid; sleep 3
echo "=== drawable dump: window manifest + leaves ==="
kill -USR2 $pid; sleep 3

xdotool key Escape
sleep 1
kill $pid 2>/dev/null
wait $pid 2>/dev/null
ls -la yserver-scanout-* yserver-drawable-* 2>/dev/null
echo "repro done"
