#!/bin/bash
# Stage 5 Task 6.1 vng smoke test: brings up yserver inside the guest,
# runs glxgears (DRI3 / PRESENT::Pixmap path) for ~30 s, captures the
# deferred-PRESENT-completion telemetry rollups.
#
# Run from the host with:
#   vng -r /boot/vmlinuz-linux-cachyos --disable-microvm --rw \
#     --qemu-opts="-display egl-headless,gl=on -vga none \
#       -device virtio-vga-gl,hostmem=4G,blob=true,venus=true,xres=1280,yres=720 \
#       -device virtio-tablet-pci -device virtio-keyboard-pci" \
#     -- bash tools/vng-defpresent-smoke.sh
set -u
cd /home/jos/Projects/yserver

mkdir -p /tmp/.X11-unix
rm -f yserver-vng.log glxgears-vng.log yserver-vng.submit.tsv

xdg_rd=$(mktemp -d -t yserver-vng.XXXXXX)
chmod 700 "$xdg_rd"

echo "=== STARTING yserver ==="
YSERVER_LOOP_TELEMETRY=1 \
    YSERVER_SUBMIT_TRACE=yserver-vng.submit.tsv \
    MESA_LOADER_DRIVER_OVERRIDE=zink \
    XDG_RUNTIME_DIR="$xdg_rd" \
    RUST_LOG=info,yserver_core::core_loop::process_request=debug,yserver::kms::render::backend=debug RUST_BACKTRACE=1 \
    target/release/yserver > yserver-vng.log 2>&1 &
ys_pid=$!

for i in $(seq 1 30); do [ -S /tmp/.X11-unix/X7 ] && break; sleep 1; done
if [ ! -S /tmp/.X11-unix/X7 ]; then
    echo "FAIL: X socket :7 never came up"
    tail -30 yserver-vng.log
    kill -9 $ys_pid 2>/dev/null
    exit 1
fi
echo "X socket up after ${i}s"

# Confirm GLX is available before launching glxgears
DISPLAY=:7 glxinfo -B 2>&1 | head -10 > glxinfo.log
echo "=== glxinfo head ==="
cat glxinfo.log
echo "==="

echo "=== STARTING glxgears (30s) ==="
DISPLAY=:7 \
    MESA_LOADER_DRIVER_OVERRIDE=zink \
    LIBGL_DRI3_DISABLE=0 \
    timeout 30 glxgears -info > glxgears-vng.log 2>&1 &
gg_pid=$!

wait $gg_pid 2>/dev/null
echo "glxgears done"

echo "=== SHUTTING DOWN yserver ==="
kill -TERM $ys_pid 2>/dev/null
wait $ys_pid 2>/dev/null
rm -rf "$xdg_rd"

echo "=== glxgears summary ==="
head -20 glxgears-vng.log
echo "..."
grep -E "FPS|frames in" glxgears-vng.log | tail -5

echo "=== TELEMETRY ROLLUPS (last 30 render_telemetry lines) ==="
grep "render_telemetry" yserver-vng.log | tail -30
echo "=== END ==="
echo "Full log: yserver-vng.log ($(wc -l < yserver-vng.log) lines)"
