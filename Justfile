KERNEL := "/boot/vmlinuz-linux-cachyos"

# ============================== SETUP & ENVIRONMENT CHECKS ==============================

# Build a release yserver and install it to /usr/local/bin (needs sudo).
install:
    cargo build --release --bin yserver
    sudo install -m755 target/release/yserver /usr/local/bin/yserver
    @echo "installed /usr/local/bin/yserver — see README 'Use with a display manager' to enable it"

# ============================== CORE — RUN / HEADLESS / SSH / DEBUG / ENTRY ==============================

# Run yserver in virtme-ng with virtio-gpu DRM device and a QEMU window.
yserver:
    cargo build --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-display gtk -vga none -device virtio-gpu-pci -device virtio-tablet-pci -device virtio-keyboard-pci" \
        -- target/debug/yserver

# Run yserver inside the guest for `seconds`, then send SIGTERM
# from inside the guest. Exercises the signalfd shutdown path.
yserver-headless-shutdown seconds="3":
    cargo build --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-device virtio-gpu-pci" \
        -- bash -c 'target/debug/yserver & pid=$!; sleep {{seconds}}; kill -TERM $pid; wait $pid'

yserver-hw log="warn":
    cargo build --release --bin yserver
    bash -c '\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/release/yserver 7 > yserver-hw.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        env DISPLAY=":7" xterm -geometry 100x80-100+0;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null'

# Picks the lowest free X display by scanning /tmp/.X11-unix/, brings
# yserver up there, then runs ~/.xinitrc (or /etc/X11/xinit/xinitrc
# fallback) with the matching DISPLAY. When xinitrc exits, yserver is
# torn down. WAYLAND_* are unset belt-and-braces; a real VT wouldn't
# have them set anyway. Rejects pty / SSH / graphical-terminal callers
# via a /dev/ttyN check on stdin — mirrors real `startx`.
#
# Runs STANDALONE from a bare TTY, so (unlike the `-hw` desktop
# recipes) it does NOT override XDG_RUNTIME_DIR — it inherits the TTY
# login's real /run/user/UID + systemd --user instance. That is what
# makes gcr-ssh-agent (and the keyring-unlocked SSH keys) reachable in
# the session with no extra wiring here: ~/.xinitrc must NOT repoint
# XDG_RUNTIME_DIR at a temp dir (an x11trace setup once did, which
# pointed SSH_AUTH_SOCK at a dead /tmp/.../gcr/ssh).
startx log="warn":
    cargo build --release --bin yserver
    bash -c '\
        case "$(tty)" in /dev/tty[0-9]*) ;; *) echo "startx: must be run from a TTY (got: $(tty))" >&2; exit 1;; esac;\
        display=0;\
        while [ -e /tmp/.X11-unix/X$display ]; do display=$((display+1)); done;\
        echo "startx: using DISPLAY=:$display";\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/release/yserver "$display" > yserver-hw-startx.log 2>&1 &\
        yserver_pid=$!;\
        for i in $(seq 30); do [ -S /tmp/.X11-unix/X$display ] && break; sleep 1; done;\
        xinitrc=~/.xinitrc;\
        [ -f "$xinitrc" ] || xinitrc=/etc/X11/xinit/xinitrc;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET XDG_SESSION_TYPE=x11 DISPLAY=":$display" sh "$xinitrc";\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null'

# Run yserver headless + wait 8 s + start xterm inside the guest.
# Use to smoke-test the xterm path without needing two terminals.
yserver-xterm:
    cargo build --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-device virtio-gpu-pci" \
        -- bash -c 'RUST_LOG=info RUST_BACKTRACE=1 target/debug/yserver &\
            yserver_pid=$!;\
            sleep 8;\
            DISPLAY=:7 xterm -e "echo xterm connected; sleep 10" &\
            wait $yserver_pid'

# ============================== GPU / APP SMOKE ==============================

# Phase 4.2 GLX smoke. glxgears exercises GLX framing + DRI3 +
# Present.
yserver-glxgears mode="1024x768" log="info":
    cargo build --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-display gtk,gl=on -vga none -device virtio-vga-gl,hostmem=4G,blob=true,venus=true,xres=1024,yres=768 -device virtio-tablet-pci -device virtio-keyboard-pci" \
        -- bash -c '\
            export VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.json;\
            export MESA_LOADER_DRIVER_OVERRIDE=zink;\
            RUST_LOG="{{log}}" RUST_BACKTRACE=1 YSERVER_MODE={{mode}} target/debug/yserver > yserver.log 2>&1 &\
            yserver_pid=$!;\
            for i in $(seq 30); do if [ -e /tmp/.X11-unix/X7 ]; then break; fi; sleep 1; done;\
            DISPLAY=:7 timeout 10 glxgears > glxgears.log 2>&1;\
            echo "===GLXGEARS rc=$?===";\
            sleep 1;\
            kill $yserver_pid 2>/dev/null;\
            wait $yserver_pid 2>/dev/null;\
            echo "===YSERVER LOG TAIL===";\
            tail -50 yserver.log;\
            echo "===GLXGEARS LOG===";\
            cat glxgears.log'

# Phase 4.2 smoke: yserver + vkcube under Venus passthrough.
# Verifies DRI3 / Present extension discovery + handshake.
#
# Pin VK_DRIVER_FILES to virtio_icd.json so the loader doesn't
# probe radeon_icd inside the guest (no PCI passthrough → spurious
# segfault at vkCreateInstance time on some stacks).
#
# Wait for /tmp/.X11-unix/X7 to materialise before launching the
# client (yserver's modeset takes ~20s under the cold-cache vng
# boot). vkcube --c N exits after N frames; we use 5.
yserver-vkcube mode="1024x768" log="info" frames="5":
    cargo build --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-display gtk,gl=on -vga none -device virtio-vga-gl,hostmem=4G,blob=true,venus=true,xres=1024,yres=768 -device virtio-tablet-pci -device virtio-keyboard-pci" \
        -- bash -c '\
            export VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.json;\
            RUST_LOG="{{log}}" RUST_BACKTRACE=1 YSERVER_MODE={{mode}} target/debug/yserver > yserver.log 2>&1 &\
            yserver_pid=$!;\
            for i in $(seq 30); do if [ -e /tmp/.X11-unix/X7 ]; then break; fi; sleep 1; done;\
            DISPLAY=:7 vkcube --c {{frames}} > vkcube.log 2>&1;\
            echo "===VKCUBE rc=$?===";\
            sleep 1;\
            kill $yserver_pid 2>/dev/null;\
            wait $yserver_pid 2>/dev/null;\
            echo "===YSERVER LOG TAIL===";\
            tail -50 yserver.log;\
            echo "===VKCUBE LOG===";\
            cat vkcube.log'

# Phase 4.1: yserver under virtio-gpu Venus passthrough.
# Exposes a real Vulkan device inside the guest. Requires
# `vulkan-virtio` on the host (Venus ICD).
yserver-venus mode="1024x768" log="info":
    cargo build --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-display gtk,gl=on -vga none -device virtio-vga-gl,hostmem=4G,blob=true,venus=true -device virtio-tablet-pci -device virtio-keyboard-pci" \
        -- bash -c 'RUST_LOG="{{log}}" RUST_BACKTRACE=1 YSERVER_MODE={{mode}} target/debug/yserver'

# ============================== CINNAMON ==============================

yserver-cinnamon-hw log="warn":
    cargo build --release --bin yserver
    bash -c '\
        xdg_rd=$(mktemp -d -t yserver-run.XXXXXX); chmod 700 "$xdg_rd";\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/release/yserver > yserver-hw-cinnamon.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:7 GDK_BACKEND=x11 \
            XDG_SESSION_TYPE=x11 XDG_RUNTIME_DIR="$xdg_rd" \
            dbus-run-session cinnamon-session > cinnamon.log 2>&1;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;\
        rm -rf "$xdg_rd" 2>/dev/null'

# Release-mode cinnamon wrapped in system-wide `perf record` (see
# tools/profile-mate.sh). For triaging cinnamon choppiness — note that on
# drivers where GLX_EXT_texture_from_pixmap isn't advertised (NVIDIA
# proprietary), muffin composites via the read-pixmap -> glTexImage2D
# fallback, so yserver's GetImage / GPU-readback path is the prime hot-spot
# to look for in the flamegraph. cinnamon-session takes DISPLAY from the
# env (no --display flag), matching `yserver-cinnamon-hw`.
yserver-cinnamon-hw-perf log="warn" freq="999":
    RUST_LOG={{log}} PERF_FREQ={{freq}} \
        SESSION_NAME=cinnamon SESSION_COMMAND=cinnamon-session \
        tools/profile-mate.sh

yserver-cinnamon-hw-telemetry log="info":
    RUSTFLAGS="-C force-frame-pointers=yes" cargo build --release --bin yserver
    rm -f yserver-cinnamon.submit.tsv
    bash -c '\
        xdg_rd=$(mktemp -d -t yserver-run.XXXXXX); chmod 700 "$xdg_rd";\
        YSERVER_LOOP_TELEMETRY=1 YSERVER_SUBMIT_TRACE=yserver-cinnamon.submit.tsv \
            RUST_LOG="{{log}}" RUST_BACKTRACE=1 \
            target/release/yserver > yserver-hw-cinnamon.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:7 GDK_BACKEND=x11 \
            XDG_SESSION_TYPE=x11 XDG_RUNTIME_DIR="$xdg_rd" \
            dbus-run-session cinnamon-session > cinnamon.log 2>&1;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;\
        rm -rf "$xdg_rd" 2>/dev/null;'

yserver-cinnamon-hw-trace log="warn,yserver::input::clickhit=trace,yserver::input::restack=trace,yserver::input::focus=trace":
    cargo build --bin yserver
    rm -f cinnamon.xtrace
    bash -c '\
        xdg_rd=$(mktemp -d -t yserver-run.XXXXXX); chmod 700 "$xdg_rd";\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/debug/yserver > yserver-hw-cinnamon.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        x11trace -d :7 -D :8 -n -o cinnamon.xtrace &\
        xtrace_pid=$!;\
        sleep 1;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:8 GDK_BACKEND=x11 \
            XDG_SESSION_TYPE=x11 XDG_RUNTIME_DIR="$xdg_rd" \
            dbus-run-session cinnamon-session > cinnamon.log 2>&1;\
        kill -TERM $xtrace_pid $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;\
        rm -rf "$xdg_rd" 2>/dev/null;'

# Same probe under a full Cinnamon session (the DE the original freeze + storm
# repros used). Release build + YSERVER_LOOP_TELEMETRY (compose rate) + xi-device
# logging + udev input monitor. Lets us check, in one run: (a) does Cinnamon idle
# to compose=0, (b) does the mouse re-acquire on monitor off->on, (c) does the
# storm recur. Let Cinnamon FULLY settle (~20s) before judging idle; then power
# the monitor OFF ~5s / ON / move mouse; then log out or zap.
#   watch: target/yserver-cinnamon-probe.log (RATE + xi-device) + target/udev-input.log
yserver-cinnamon-hotplug-probe log="info":
    cargo build --release --bin yserver
    rm -f target/udev-input.log target/yserver-cinnamon-probe.log
    bash -c '\
        xdg_rd=$(mktemp -d -t yserver-run.XXXXXX); chmod 700 "$xdg_rd";\
        ( stdbuf -oL udevadm monitor --udev --kernel --subsystem-match=input 2>&1 | while IFS= read -r l; do printf "%s %s\n" "$(date -u +%H:%M:%S.%3N)" "$l"; done > target/udev-input.log ) &\
        udev_pid=$!;\
        YSERVER_LOOP_TELEMETRY=1 RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/release/yserver > target/yserver-cinnamon-probe.log 2>&1 &\
        yserver_pid=$!;\
        for i in $(seq 100); do [ -S /tmp/.X11-unix/X7 ] && break; sleep 0.1; done;\
        echo "yserver up; starting Cinnamon. Let it settle ~20s, check idle, then power-cycle the monitor + move mouse. Log out / zap to stop.";\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:7 GDK_BACKEND=x11 \
            XDG_SESSION_TYPE=x11 XDG_RUNTIME_DIR="$xdg_rd" \
            dbus-run-session cinnamon-session > cinnamon.log 2>&1;\
        kill -TERM $yserver_pid $udev_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;\
        rm -rf "$xdg_rd" 2>/dev/null;'

# ============================== MATE ==============================

yserver-mate mode="1024x768" log="trace":
    cargo build --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-display gtk,gl=on -vga none -device virtio-vga-gl,hostmem=4G,blob=true,venus=true,xres=1024,yres=768 -device virtio-tablet-pci -device virtio-keyboard-pci" \
        -- bash -c '\
            export MESA_LOADER_DRIVER_OVERRIDE=zink;\
            RUST_LOG="{{log}}" RUST_BACKTRACE=1 YSERVER_MODE={{mode}} target/debug/yserver > yserver.log 2>&1 &\
            yserver_pid=$!;\
            for i in $(seq 30); do if [ -e /tmp/.X11-unix/X7 ]; then break; fi; sleep 1; done;\
            env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:7 GDK_BACKEND=x11 \
                XDG_SESSION_TYPE=x11 \
                dbus-run-session mate-session --display :7 > mate.log 2>&1 &\
            wait $yserver_pid'

yserver-mate-hw log="warn":
    cargo build --release --bin yserver
    bash -c '\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/release/yserver > yserver-hw-mate.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:7 GDK_BACKEND=x11 \
            XDG_SESSION_TYPE=x11 \
            dbus-run-session mate-session --display :7 > mate.log 2>&1;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null'

# Release-mode mate wrapped in system-wide `perf record`. See
# tools/profile-mate.sh for what it captures and how to read the trace.
# Set `STRACE=1` in the env to also attach strace to caja the moment it
# spawns (writes caja.strace; useful for "what is caja sitting in poll()
# on for 25s").
yserver-mate-hw-perf log="warn" freq="999":
    RUST_LOG={{log}} PERF_FREQ={{freq}} tools/profile-mate.sh

# Release-mode mate with logging turned down to `warn`. Use this to
# test whether pointer lag under hover is dominated by env_logger /
# stderr formatting cost (observed at ~5% of CPU under debug+debug
# build) or by the underlying paint pipeline. If hover responds
# noticeably faster than `yserver-mate-hw`, logging was the bottleneck.
#
# Build is forced with `-C force-frame-pointers=yes` so that
# `perf record --call-graph fp` can walk the stack reliably for
# flamegraphs. Without this, optimized Rust release builds produce
# ~66% [unknown] frames in the flamegraph (DWARF unwinding fails
# partway through inlined call chains). ~1-2% runtime cost; harmless
# for general release use, essential for profiling.
yserver-mate-hw-release log="warn":
    RUSTFLAGS="-C force-frame-pointers=yes" cargo build --release --bin yserver
    bash -c '\
        xdg_rd=$(mktemp -d -t yserver-run.XXXXXX); chmod 700 "$xdg_rd";\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/release/yserver > yserver-hw-mate.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:7 GDK_BACKEND=x11 \
            XDG_SESSION_TYPE=x11 XDG_RUNTIME_DIR="$xdg_rd" \
            dbus-run-session mate-session --display :7 > mate.log 2>&1;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;\
        rm -rf "$xdg_rd" 2>/dev/null;'

# Release-build counterpart to `yserver-mate-hw-trace`: builds with
# `--release` (so perf characteristics match real-world) but still
# wires `x11trace` between mate-session and yserver, dumping the
# protocol stream to `mate.xtrace`. Use when comparing wire-level
# behaviour to `mate-xephyr-trace`'s `mate-xorg.xtrace` — the trace
# recipe above produces a debug-built log that is ~3-5× slower per
# request, which can mask or distort timing-related symptoms.
#
# Defaults `RUST_LOG=warn` so yserver-hw-mate.log stays compact; pass
# `log=...` to crank specific targets for a cross-reference run.
yserver-mate-hw-release-trace log="warn":
    RUSTFLAGS="-C force-frame-pointers=yes" cargo build --release --bin yserver
    rm -f mate.xtrace
    bash -c '\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/release/yserver > yserver-hw-mate.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        x11trace -d :7 -D :8 -n -o mate.xtrace &\
        xtrace_pid=$!;\
        sleep 1;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:8 GDK_BACKEND=x11 \
            XDG_SESSION_TYPE=x11 \
            dbus-run-session mate-session --display :8 > mate.log 2>&1;\
        kill -TERM $xtrace_pid $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;'

# Release-mode mate with the core-loop telemetry enabled (see
# `LoopTelemetry` in `crates/yserver-core/src/core_loop/run.rs`).
# Emits one info!-level line per second to yserver-hw.log with
# iter/s, req/s, drain_max, top opcodes, host_input gap, etc.
#
# Also writes a per-vkQueueSubmit2 TSV to `yserver-${session}.submit.tsv`
# (Stage 5 Task 3 paint-aggregation diagnostic, see
# crates/yserver/src/kms/v2/submit_trace.rs). One row per submit:
#   frame_id ns_mono kind target_kind target_id batch_size op \
#   src_class mask_class pipeline_id readback alias zero_draws upload
# Quick analyses:
#   awk -F'\t' 'NR>1{c[$3]++} END{for(k in c) print c[k],k}' \
#       yserver-mate.submit.tsv | sort -rn
#   awk -F'\t' 'NR>1 && $3==pk && $5==pt {run++; next} \
#       {if(run>1) print run,pk,pt; run=1; pk=$3; pt=$5}' \
#       yserver-mate.submit.tsv | sort -rn | head
#
# Use to diagnose input-loop starvation on bee/adapta — reproduce
# the lag, then `grep "loop telemetry" yserver-hw.log` for the
# rollups. RUST_LOG defaults to `info` so the telemetry lines come
# through; pass `log=warn` if you need quieter output, but you'll
# lose the rollup lines (they're info!-level).
yserver-mate-hw-telemetry log="info":
    RUSTFLAGS="-C force-frame-pointers=yes" cargo build --release --bin yserver
    rm -f yserver-mate.submit.tsv
    bash -c '\
        xdg_rd=$(mktemp -d -t yserver-run.XXXXXX); chmod 700 "$xdg_rd";\
        YSERVER_LOOP_TELEMETRY=1 YSERVER_SUBMIT_TRACE=yserver-mate.submit.tsv \
            RUST_LOG="{{log}}" RUST_BACKTRACE=1 \
            target/release/yserver > yserver-hw-mate.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:7 GDK_BACKEND=x11 \
            XDG_SESSION_TYPE=x11 XDG_RUNTIME_DIR="$xdg_rd" \
            dbus-run-session mate-session --display :7 > mate.log 2>&1;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;\
        rm -rf "$xdg_rd" 2>/dev/null;'

# MATE on yserver/KMS with x11trace recording the full X11 wire
# protocol between clients and yserver. Follows the server default
# cursor strategy, currently SW cursor.
yserver-mate-hw-trace log="debug,yserver_core::core_loop::damage_fanout=trace,yserver::xfixes::clip=trace,yserver::xfixes::region=trace,yserver::kms::v2::render=trace":
    cargo build --bin yserver
    rm -f mate.xtrace
    bash -c '\
        xdg_rd=$(mktemp -d -t yserver-run.XXXXXX); chmod 700 "$xdg_rd";\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 \
            YSERVER_V2_SCENE_WALK_ALL=1 \
            target/debug/yserver > yserver-hw-mate.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        x11trace -d :7 -D :8 -n -o mate.xtrace &\
        xtrace_pid=$!;\
        sleep 1;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:8 GDK_BACKEND=x11 \
            XDG_SESSION_TYPE=x11 XDG_RUNTIME_DIR="$xdg_rd" \
            dbus-run-session mate-session --display :8 > mate.log 2>&1;\
        kill -TERM $xtrace_pid $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;\
        rm -rf "$xdg_rd" 2>/dev/null;'

# Counterpart to `yserver-mate-hw` with Vulkan validation + RADV
# hang reporting wired in for tracking down GPU VM faults / device
# losses. Use when yserver wedges with `ERROR_DEVICE_LOST` on a
# RADV-driven AMD card/APU:
#   - YSERVER_VK_VALIDATION=1 + VK_INSTANCE_LAYERS turns on the
#     Khronos validation layer (needs `vulkan-validation-layers`
#     installed; the loader will warn-and-continue if absent).
#   - VK_LAYER_ENABLES=...SYNCHRONIZATION_VALIDATION_EXT pinpoints
#     missing layout/cache barriers (the most likely class of bug
#     for a TCP texture-read VM fault).
#   - RADV_DEBUG=hang,syncshaders makes RADV insert wait-idle
#     around every shader stage and dump GPU state to
#     ~/radv_dumps/ when a hang/fault fires. syncshaders is slow
#     by design — that's the point; it makes the offending submit
#     localizable.
#   - MESA_VK_ABORT_ON_DEVICE_LOSS=1 aborts the process on the
#     first device-lost rather than letting hundreds of downstream
#     RendererFailed warnings drown the actual cause.
# Writes logs to `yserver-hw-mate-vkdebug.log` so the baseline
# `yserver-hw-mate.log` is preserved for diffing.
yserver-mate-hw-vkdebug log="debug,yserver::kms::v2::scene=trace,yserver::kms::v2::render=trace,yserver::kms::v2::fill=trace,yserver::kms::v2::store=trace,yserver::kms::v2::paint=trace":
    cargo build --bin yserver
    bash -c '\
        xdg_rd=$(mktemp -d -t yserver-run.XXXXXX); chmod 700 "$xdg_rd";\
        RUST_LOG="{{log}}" RUST_BACKTRACE=full \
            YSERVER_VK_VALIDATION=1 \
            VK_INSTANCE_LAYERS=VK_LAYER_KHRONOS_validation \
            VK_LAYER_ENABLES=VK_VALIDATION_FEATURE_ENABLE_SYNCHRONIZATION_VALIDATION_EXT \
            RADV_DEBUG=hang,syncshaders \
            MESA_VK_ABORT_ON_DEVICE_LOSS=1 \
            target/debug/yserver > yserver-hw-mate-vkdebug.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:7 GDK_BACKEND=x11 \
            XDG_SESSION_TYPE=x11 XDG_RUNTIME_DIR="$xdg_rd" \
            dbus-run-session mate-session --display :7 > mate-vkdebug.log 2>&1;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;\
        rm -rf "$xdg_rd" 2>/dev/null;\
        echo "yserver log: yserver-hw-mate-vkdebug.log";\
        echo "mate log:    mate-vkdebug.log";\
        echo "radv dumps:  ~/radv_dumps/ (if any)";'

# MATE inside Xephyr (nested Xorg-family server), with x11trace
# recording marco's wire stream to/from Xephyr. The Xorg-side
# counterpart to `yserver-mate-hw-trace`: same workload, same
# tracer, but server is Xephyr (kdrive/ephyr, shares dix with
# Xorg) instead of yserver. Compare `mate-xorg.xtrace` against
# `mate.xtrace` to find the divergent server reply/event that
# makes marco's compositor logic take a different branch.
#
# Layout:
#   - Xephyr on :18 — outer X server for the nested session.
#   - x11trace tunnels :18 → :19, dumping wire to mate-xorg.xtrace.
#   - mate-session connects to :19 (sees x11trace as its server).
#
# Works under GNOME-Wayland: Xephyr opens as a regular window
# managed by mutter; mate-session inside is fully isolated from
# the host session (its own dbus via dbus-run-session). Focus the
# Xephyr window to send input. Ctrl-Shift releases pointer grab
# if Xephyr captures it.
#
# Defaults to 5120x1440 to match the yserver hardware-scanout
# scenario so CC's drag distances and geometry are comparable.
mate-xephyr-trace screen="1920x1080":
    rm -f mate-xorg.xtrace mate-xephyr.log mate-xorg.log
    bash -c 'set -e;\
        if [[ -z "${DISPLAY:-}" ]]; then echo "need a host DISPLAY (XWayland under GNOME provides one)" >&2; exit 1; fi;\
        if ! command -v Xephyr >/dev/null; then echo "Xephyr not installed (pacman -S xorg-server-xephyr)" >&2; exit 1; fi;\
        if ! command -v x11trace >/dev/null; then echo "x11trace not installed (pacman -S xtrace)" >&2; exit 1; fi;\
        xdg_rd=$(mktemp -d -t yserver-run.XXXXXX); chmod 700 "$xdg_rd";\
        echo "outer DISPLAY=$DISPLAY  nested=:18  traced=:19  XDG_RUNTIME_DIR=$xdg_rd";\
        Xephyr -screen {{screen}} -title "mate-xorg-trace" :18 > mate-xephyr.log 2>&1 &\
        xephyr_pid=$!;\
        trap "kill -TERM $xephyr_pid 2>/dev/null; wait $xephyr_pid 2>/dev/null; rm -rf $xdg_rd" EXIT;\
        for _ in $(seq 1 50); do [[ -S /tmp/.X11-unix/X18 ]] && break; sleep 0.1; done;\
        if [[ ! -S /tmp/.X11-unix/X18 ]]; then echo "Xephyr :18 never came up; see mate-xephyr.log" >&2; tail -20 mate-xephyr.log >&2; exit 2; fi;\
        x11trace -d :18 -D :19 -n -o mate-xorg.xtrace &\
        xtrace_pid=$!;\
        trap "kill -TERM $xtrace_pid $xephyr_pid 2>/dev/null; wait $xephyr_pid 2>/dev/null; rm -rf $xdg_rd" EXIT;\
        sleep 1;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:19 GDK_BACKEND=x11 \
            XDG_SESSION_TYPE=x11 XDG_RUNTIME_DIR="$xdg_rd" \
            dbus-run-session mate-session --display :19 > mate-xorg.log 2>&1;\
        echo "Xephyr log: mate-xephyr.log";\
        echo "x11trace:   mate-xorg.xtrace";\
        echo "mate log:   mate-xorg.log"'

# ============================== XFCE ==============================

yserver-xfce-hw log="warn":
    cargo build --release --bin yserver
    bash -c '\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/release/yserver > yserver-hw-xfce.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:7 GDK_BACKEND=x11 YSERVER_V2_SCENE_WALK_ALL=1\
            XDG_SESSION_TYPE=x11 \
            dbus-run-session xfce4-session --display :7 > xfce.log 2>&1;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null'

yserver-xfce-hw-perf log="warn" freq="999":
    RUST_LOG={{log}} PERF_FREQ={{freq}} \
        SESSION_NAME=xfce SESSION_COMMAND='xfce4-session --display :7' \
        tools/profile-mate.sh

yserver-xfce-hw-telemetry log="info":
    RUSTFLAGS="-C force-frame-pointers=yes" cargo build --release --bin yserver
    rm -f yserver-xfce.submit.tsv
    bash -c '\
        YSERVER_LOOP_TELEMETRY=1 YSERVER_SUBMIT_TRACE=yserver-xfce.submit.tsv \
            RUST_LOG="{{log}}" RUST_BACKTRACE=1 \
            target/release/yserver > yserver-hw-xfce.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:7 GDK_BACKEND=x11 \
            XDG_SESSION_TYPE=x11 \
            dbus-run-session xfce4-session --display :7 > xfce.log 2>&1;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;'

# DIAGNOSTIC (#48): find WHY the desktop layer (xfdesktop / nemo-desktop)
# is slow-or-black under yserver. Launches yserver with request-level debug
# logging + xfce, and the moment the desktop client appears, attaches
# `strace` to it so we can see what it BLOCKS on:
#   recvmsg on the yserver socket -> waiting on an X reply (yserver's fault)
#   poll/recvmsg on a dbus socket -> dbus activation/timeout
#   ioctl on /dev/dri or futex    -> client-side Mesa/GL (e.g. RDNA4)
# Let the session sit at the black/slow desktop for ~15s, then log out.
# Attach ALL THREE files to the issue:
#   yserver-hw-xfce.log    (yserver, incl. per-request debug)
#   desktop-client.strace  (what the desktop client did / blocked on)
#   desktop-client.wchan   (per-thread kernel wait-channel snapshots)
# Needs `strace` (Debian/PikaOS: sudo apt install strace; Arch: pacman -S
# strace). The watcher attaches strace to the desktop client, which is a
# SIBLING process — that requires kernel.yama.ptrace_scope=0 (scope=1 only
# allows an ANCESTOR to trace). The recipe checks this and tells you the
# one-time fix if needed.
yserver-xfce-hw-strace log="info,yserver_core::core_loop::process_request=debug":
    cargo build --release --bin yserver
    @command -v strace >/dev/null 2>&1 || { echo "ERROR: strace not installed (Debian/PikaOS: sudo apt install strace; Arch: sudo pacman -S strace)"; exit 1; }
    @[ "$(cat /proc/sys/kernel/yama/ptrace_scope 2>/dev/null || echo 0)" = "0" ] || { echo "ERROR: kernel.yama.ptrace_scope != 0 — strace can't attach to the (sibling) desktop client. Fix for this boot: sudo sysctl kernel.yama.ptrace_scope=0  (resets on reboot), then re-run."; exit 1; }
    rm -f yserver-hw-xfce.log desktop-client.strace desktop-client.wchan
    bash -c '\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 \
            target/release/yserver > yserver-hw-xfce.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        ( pid="";\
          for i in $(seq 1 120); do \
              pid=$(pgrep -x xfdesktop | head -1);\
              [ -z "$pid" ] && pid=$(pgrep -x nemo-desktop | head -1);\
              [ -n "$pid" ] && break;\
              sleep 0.25;\
          done;\
          if [ -z "$pid" ]; then \
              echo "desktop client (xfdesktop/nemo-desktop) never appeared within 30s" > desktop-client.strace;\
          else \
              echo "attached to desktop client pid=$pid ($(cat /proc/$pid/comm 2>/dev/null))";\
              ( for s in $(seq 1 20); do \
                  echo "=== t+${s}s ===" >> desktop-client.wchan;\
                  for w in /proc/$pid/task/*/wchan; do \
                      echo "  tid $(basename $(dirname $w)): $(cat $w 2>/dev/null)" >> desktop-client.wchan;\
                  done;\
                  sleep 1;\
              done ) &\
              wchan_pid=$!;\
              strace -f -tt -T -y -s 512 -p "$pid" 2> desktop-client.strace;\
              kill $wchan_pid 2>/dev/null;\
          fi ) &\
        watcher_pid=$!;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:7 GDK_BACKEND=x11 \
            XDG_SESSION_TYPE=x11 \
            dbus-run-session xfce4-session --display :7 > xfce.log 2>&1;\
        kill -TERM $yserver_pid 2>/dev/null;\
        pkill -P $watcher_pid 2>/dev/null; kill $watcher_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;\
        echo "DONE — attach to issue #48: yserver-hw-xfce.log desktop-client.strace desktop-client.wchan";'

# xfce on yserver with x11trace recording the full X11 wire
# protocol between clients and yserver. xfce-session connects to
# the fake display `:8`; x11trace tunnels everything to yserver
# on `:7` and dumps a human-readable per-request/per-event trace
# to `xfce.xtrace`. Use to diff against an Xorg-side capture
# (see `xfce-xorg-trace`) when debugging GTK popup placement,
# rubber-band selection, or any "works on Xorg, broken on
# yserver" client-side bug.
yserver-xfce-hw-trace log="debug":
    cargo build --bin yserver
    rm -f xfce.xtrace
    bash -c '\
        xdg_rd=$(mktemp -d -t yserver-run.XXXXXX); chmod 700 "$xdg_rd";\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/debug/yserver > yserver-hw-xfce.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        x11trace -d :7 -D :8 -n -o xfce.xtrace &\
        xtrace_pid=$!;\
        sleep 1;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:8 GDK_BACKEND=x11 \
            XDG_SESSION_TYPE=x11 XDG_RUNTIME_DIR="$xdg_rd" \
            dbus-run-session xfce4-session --display :8 > xfce.log 2>&1;\
        kill -TERM $xtrace_pid $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;\
        rm -rf "$xdg_rd" 2>/dev/null;'

xfce-xephyr-trace screen="1920x1080":
    rm -f xfce-xorg.xtrace xfce-xephyr.log xfce-xorg.log
    bash -c 'set -e;\
        if [[ -z "${DISPLAY:-}" ]]; then echo "need a host DISPLAY (XWayland under GNOME provides one)" >&2; exit 1; fi;\
        if ! command -v Xephyr >/dev/null; then echo "Xephyr not installed (pacman -S xorg-server-xephyr)" >&2; exit 1; fi;\
        if ! command -v x11trace >/dev/null; then echo "x11trace not installed (pacman -S xtrace)" >&2; exit 1; fi;\
        xdg_rd=$(mktemp -d -t yserver-run.XXXXXX); chmod 700 "$xdg_rd";\
        echo "outer DISPLAY=$DISPLAY  nested=:18  traced=:19  XDG_RUNTIME_DIR=$xdg_rd";\
        Xephyr -screen {{screen}} -title "xfce-xorg-trace" :18 > xfce-xephyr.log 2>&1 &\
        xephyr_pid=$!;\
        trap "kill -TERM $xephyr_pid 2>/dev/null; wait $xephyr_pid 2>/dev/null; rm -rf $xdg_rd" EXIT;\
        for _ in $(seq 1 50); do [[ -S /tmp/.X11-unix/X18 ]] && break; sleep 0.1; done;\
        if [[ ! -S /tmp/.X11-unix/X18 ]]; then echo "Xephyr :18 never came up; see xfce-xephyr.log" >&2; tail -20 xfce-xephyr.log >&2; exit 2; fi;\
        x11trace -d :18 -D :19 -n -o xfce-xorg.xtrace &\
        xtrace_pid=$!;\
        trap "kill -TERM $xtrace_pid $xephyr_pid 2>/dev/null; wait $xephyr_pid 2>/dev/null; rm -rf $xdg_rd" EXIT;\
        sleep 1;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:19 GDK_BACKEND=x11 \
            XDG_SESSION_TYPE=x11 XDG_RUNTIME_DIR="$xdg_rd" \
            dbus-run-session xfce4-session --display=:19 > xfce-xorg.log 2>&1;\
        echo "Xephyr log: xfce-xephyr.log";\
        echo "x11trace:   xfce-xorg.xtrace";\
        echo "xfce log:   xfce-xorg.log"'

# ============================== ENLIGHTENMENT (e16) ==============================

yserver-e16-xterm mode="1024x768" log="trace":
    cargo build --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-display gtk,gl=on -vga none -device virtio-vga-gl,hostmem=4G,blob=true,venus=true,xres=1024,yres=768 -device virtio-tablet-pci -device virtio-keyboard-pci" \
        -- bash -c '\
            export MESA_LOADER_DRIVER_OVERRIDE=zink;\
            RUST_LOG="{{log}}" RUST_BACKTRACE=1 YSERVER_MODE={{mode}} target/debug/yserver > yserver.log 2>&1 &\
            yserver_pid=$!;\
            sleep 3;\
            DISPLAY=:7 e16 > e16.log 2>&1 &\
            sleep 3;\
            DISPLAY=:7 xterm &\
            wait $yserver_pid'

yserver-e16-xterm-hw log="debug":
    cargo build --bin yserver
    bash -c '\
        unset WAYLAND_DISPLAY WAYLAND_SOCKET;\
        export GDK_BACKEND=x11;\
        export XDG_SESSION_TYPE=x11;\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 YSERVER_OPS_SAFE=1 target/debug/yserver > yserver-hw-e16.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        DISPLAY=:7 e16 > e16-hw.log 2>&1 &\
        sleep 2;\
        DISPLAY=:7 wezterm;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;'

# e16 + wezterm on yserver with x11trace recording the X11 wire
# protocol between clients and yserver. e16 connects to the fake
# display `:8`; x11trace tunnels everything to yserver on `:7` and
# dumps a human-readable per-request/per-event trace to `e16.xtrace`.
# Use to diff against an Xorg-side capture when debugging e16
# hover-popup gating or other event-flow oddities.
yserver-e16-xterm-hw-trace log="debug":
    cargo build --bin yserver
    rm -f e16.xtrace
    bash -c '\
        unset WAYLAND_DISPLAY WAYLAND_SOCKET;\
        export GDK_BACKEND=x11;\
        export XDG_SESSION_TYPE=x11;\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 YSERVER_OPS_SAFE=1 target/debug/yserver > yserver-hw-e16.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        x11trace -d :7 -D :8 -n -o e16.xtrace &\
        xtrace_pid=$!;\
        sleep 1;\
        DISPLAY=:8 e16 > e16-hw.log 2>&1 &\
        sleep 2;\
        DISPLAY=:8 wezterm;\
        kill -TERM $xtrace_pid $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;'

# Bring up yserver + e16 + wezterm under vng. wezterm exercises the
# GLX → DRI3 → Present path that was the original motivation for the
# zink override: vng's default Mesa driver is virgl, which rejects
# wezterm's GL command stream on the host (`vrend_decode_ctx_submit_cmd:
# Illegal command buffer`). Forcing `MESA_LOADER_DRIVER_OVERRIDE=zink`
# routes GL through Mesa's zink (GL→Vulkan) which then goes via Venus,
# bypassing virglrenderer entirely. wezterm under bare-metal works
# without this override because the bare-metal stack uses radeonsi/anv
# directly, not virgl.
yserver-e16-wezterm mode="1024x768" log="info":
    cargo build --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-display gtk,gl=on -vga none -device virtio-vga-gl,hostmem=4G,blob=true,venus=true,xres=1024,yres=768 -device virtio-tablet-pci -device virtio-keyboard-pci" \
        -- bash -c '\
            export VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.json;\
            export MESA_LOADER_DRIVER_OVERRIDE=zink;\
            RUST_LOG="{{log}}" RUST_BACKTRACE=1 YSERVER_MODE={{mode}} target/debug/yserver > yserver.log 2>&1 &\
            yserver_pid=$!;\
            for i in $(seq 30); do if [ -e /tmp/.X11-unix/X7 ]; then break; fi; sleep 1; done;\
            DISPLAY=:7 e16 > e16.log 2>&1 &\
            sleep 4;\
            DISPLAY=:7 wezterm &\
            wait $yserver_pid'

# Capture the e16 IDLE protocol loop (no wezterm, no interaction) — for the
# 2026-06-14 "idle never idles" investigation. Starts yserver + x11trace + e16
# ALONE, lets it settle, then traces `secs` seconds of steady idle into
# e16-idle.xtrace and tears down cleanly (self-terminating — no zap needed).
# e16's pager redraws a live desktop miniature by sampling the root (~96
# CopyArea/rebuild). On yserver this NEVER settles (~970 CopyArea/s for minutes);
# on Xorg the pager draws once and STOPS. Compare against xorg-e16-idle-trace.
# Keep `secs` small — the file grows fast at ~1000 req/s.
#     just yserver-e16-idle-trace          # 5s steady-idle capture
#     just yserver-e16-idle-trace secs=3
yserver-e16-idle-trace secs="6" log="warn":
    cargo build --bin yserver
    rm -f e16-idle.xtrace
    bash -c '\
        unset WAYLAND_DISPLAY WAYLAND_SOCKET;\
        export GDK_BACKEND=x11 XDG_SESSION_TYPE=x11;\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 YSERVER_OPS_SAFE=1 target/debug/yserver > yserver-hw-e16.log 2>&1 &\
        yserver_pid=$!;\
        for i in $(seq 100); do [ -S /tmp/.X11-unix/X7 ] && break; sleep 0.1; done;\
        x11trace -d :7 -D :8 -n -o e16-idle.xtrace >x11trace-idle.err 2>&1 &\
        xtrace_pid=$!;\
        for i in $(seq 50); do [ -S /tmp/.X11-unix/X8 ] && break; sleep 0.1; done;\
        echo "starting e16 through trace; capturing ~{{secs}}s (incl startup). DO NOT touch input.";\
        DISPLAY=:8 e16 > e16-hw.log 2>&1 &\
        e16_pid=$!;\
        sleep {{secs}};\
        kill -TERM $e16_pid $xtrace_pid $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;\
        echo "done: $(wc -l < e16-idle.xtrace 2>/dev/null) trace lines in e16-idle.xtrace";'

# REFERENCE: same e16 + pager on real Xorg, traced — the de-facto-spec compare
# for the "idle never idles" bug. Run from a free text VT (Xorg.wrap lets a
# console user start it). WATCH the screen: e16's pager draws its desktop
# miniature column-by-column, then on Xorg it should STOP within a few seconds.
# Longer default window than the yserver recipe so we confirm it settles to 0
# CopyArea (if the pager finishes, the trace stays small).
#     just xorg-e16-idle-trace           # 20s capture
#     just xorg-e16-idle-trace secs=30
xorg-e16-idle-trace secs="20":
    rm -f e16-xorg.xtrace
    bash -c '\
        unset WAYLAND_DISPLAY WAYLAND_SOCKET;\
        export GDK_BACKEND=x11 XDG_SESSION_TYPE=x11;\
        Xorg :9 -keeptty > xorg9.log 2>&1 &\
        xorg_pid=$!;\
        for i in $(seq 150); do [ -S /tmp/.X11-unix/X9 ] && break; sleep 0.1; done;\
        if [ ! -S /tmp/.X11-unix/X9 ]; then echo "FAIL: Xorg :9 did not start"; tail -20 xorg9.log; kill $xorg_pid 2>/dev/null; exit 1; fi;\
        x11trace -d :9 -D :10 -n -o e16-xorg.xtrace >x11trace-xorg.err 2>&1 &\
        xtrace_pid=$!;\
        for i in $(seq 50); do [ -S /tmp/.X11-unix/X10 ] && break; sleep 0.1; done;\
        echo "e16 on Xorg :9 via trace; capturing {{secs}}s. WATCH: does the pager stop drawing?";\
        DISPLAY=:10 e16 > e16-xorg.log 2>&1 &\
        e16_pid=$!;\
        sleep {{secs}};\
        kill -TERM $e16_pid $xtrace_pid $xorg_pid 2>/dev/null;\
        wait $xorg_pid 2>/dev/null;\
        echo "done: $(wc -l < e16-xorg.xtrace 2>/dev/null) trace lines; CopyArea=$(grep -c CopyArea e16-xorg.xtrace 2>/dev/null)";'

# ============================== FVWM3 ==============================

# Idle-rate check under fvwm3 (a quiet, non-polling WM — unlike e16's pager).
# Brings up yserver + fvwm3 with YSERVER_LOOP_TELEMETRY=1, telemetry -> the same
# target/yserver-telemetry.log we watch. Leave it idle (don't touch input) and
# the per-second "vk call rate" / "loop telemetry" lines should show
# compose=0 / req/s=0 once settled — the decisive "yserver reaches 0/s" gate
# (the cursor-damage idle fix). Zap or Ctrl-C to stop.
yserver-fvwm3-idle log="info":
    cargo build --bin yserver
    bash -c '\
        unset WAYLAND_DISPLAY WAYLAND_SOCKET;\
        export GDK_BACKEND=x11 XDG_SESSION_TYPE=x11;\
        RUST_LOG="{{log}}" YSERVER_LOOP_TELEMETRY=1 RUST_BACKTRACE=1 target/debug/yserver > target/yserver-telemetry.log 2>&1 &\
        yserver_pid=$!;\
        for i in $(seq 100); do [ -S /tmp/.X11-unix/X7 ] && break; sleep 0.1; done;\
        DISPLAY=:7 fvwm3 > target/fvwm3-idle.log 2>&1 &\
        echo "fvwm3 up on :7; telemetry -> target/yserver-telemetry.log. Leave idle; watch compose rate. Zap/Ctrl-C to stop.";\
        wait $yserver_pid 2>/dev/null;'

# Bring up yserver + fvwm3 + xterm in one QEMU window. The WM starts
# before xterm so the terminal gets framed. Logs to yserver.log on the
# host side via the shared cwd. Override resolution with `mode=WxH`.
#
# yserver runs in the background; xterm is the foreground process so
# closing it terminates the recipe (yserver dies with the guest).
yserver-fvwm3-xterm mode="1024x768" log="trace":
    cargo build --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-display gtk,gl=on -vga none -device virtio-vga-gl,hostmem=4G,blob=true,venus=true,xres=1024,yres=768 -device virtio-tablet-pci -device virtio-keyboard-pci" \
        -- bash -c '\
            export MESA_LOADER_DRIVER_OVERRIDE=zink;\
            RUST_LOG="{{log}}" RUST_BACKTRACE=1 YSERVER_MODE={{mode}} target/debug/yserver > yserver.log 2>&1 &\
            yserver_pid=$!;\
            sleep 2;\
            DISPLAY=:7 fvwm3 > fvwm3.log 2>&1 &\
            sleep 2;\
            DISPLAY=:7 xterm &\
            wait $yserver_pid'

yserver-fvwm3-xterm-hw log="debug":
    cargo build --bin yserver
    bash -c '\
        RUST_LOG="{{log}},yserver::kms::v2::scene=debug" RUST_BACKTRACE=1 target/debug/yserver > yserver-hw-fvwm3.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        DISPLAY=:7 fvwm3 > fvwm3-hw.log 2>&1 &\
        sleep 8;\
        DISPLAY=:7 wezterm;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;'

# ============================== WINDOW MAKER ==============================

yserver-wmaker-xterm mode="1024x768" log="trace":
    cargo build --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-display gtk,gl=on -vga none -device virtio-vga-gl,hostmem=4G,blob=true,venus=true,xres=1024,yres=768 -device virtio-tablet-pci -device virtio-keyboard-pci" \
        -- bash -c '\
            export MESA_LOADER_DRIVER_OVERRIDE=zink;\
            RUST_LOG="{{log}}" RUST_BACKTRACE=1 YSERVER_MODE={{mode}} target/debug/yserver > yserver.log 2>&1 &\
            yserver_pid=$!;\
            sleep 2;\
            DISPLAY=:7 wmaker > wmaker.log 2>&1 &\
            sleep 2;\
            DISPLAY=:7 xterm &\
            wait $yserver_pid'

yserver-wmaker-xterm-hw log="debug":
    cargo build --bin yserver
    bash -c '\
        xdg_rd=$(mktemp -d -t yserver-run.XXXXXX); chmod 700 "$xdg_rd";\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/debug/yserver > yserver-hw-wmaker.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:7 GDK_BACKEND=x11 YSERVER_V2_SCENE_WALK_ALL=1\
            XDG_SESSION_TYPE=x11 XDG_RUNTIME_DIR="$xdg_rd" \
            wmaker > wmaker-hw.log 2>&1 &\
        sleep 2;\
        DISPLAY=:7 wezterm;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;\
        rm -rf "$xdg_rd" 2>/dev/null;'

# wmaker + wezterm on yserver with x11trace tunnelling. wmaker connects
# to the fake display `:8`; x11trace forwards every request/event to
# yserver on `:7` and writes a per-request/per-event trace to
# `wmaker.xtrace`. Use when debugging which window/drawable wmaker is
# painting (the yserver debug log omits drawable xids on PolySegment /
# PolyFillRectangle / ClearArea); compare against an Xorg capture or
# read alongside `yserver-hw-wmaker.log`.
yserver-wmaker-xterm-hw-trace log="debug":
    cargo build --bin yserver
    rm -f wmaker.xtrace
    bash -c '\
        xdg_rd=$(mktemp -d -t yserver-run.XXXXXX); chmod 700 "$xdg_rd";\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/debug/yserver > yserver-hw-wmaker.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        x11trace -d :7 -D :8 -n -o wmaker.xtrace &\
        xtrace_pid=$!;\
        sleep 1;\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:8 GDK_BACKEND=x11 YSERVER_V2_SCENE_WALK_ALL=1\
            XDG_SESSION_TYPE=x11 XDG_RUNTIME_DIR="$xdg_rd" \
            wmaker > wmaker-hw.log 2>&1 &\
        sleep 2;\
        DISPLAY=:8 wezterm;\
        kill -TERM $xtrace_pid $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;\
        rm -rf "$xdg_rd" 2>/dev/null;'

# ============================== PROBES & MISC HW ==============================

# Input-device hotplug probe for the "mouse doesn't return after monitor
# off->on" issue (2026-06-14, vs GNOME-Wayland which recovers it). Runs yserver
# + fvwm3 AND a UTC-timestamped `udevadm monitor` of the input subsystem, so we
# can correlate WHEN the kernel re-creates the mouse node vs WHEN yserver picks
# it up. Procedure: run from a VT, then physically power the monitor OFF, wait
# ~5s, power ON, move the mouse; then zap/Ctrl-C. Compare:
#   target/yserver-hotplug.log  — yserver `xi-device`/`libinput` add/remove
#   target/udev-input.log       — kernel/udev input device add/remove
# kernel-add at screen-on but yserver-add only later => yserver hotplug bug.
yserver-input-hotplug-probe log="info":
    cargo build --bin yserver
    rm -f target/udev-input.log target/yserver-hotplug.log
    bash -c '\
        unset WAYLAND_DISPLAY WAYLAND_SOCKET;\
        export GDK_BACKEND=x11 XDG_SESSION_TYPE=x11;\
        ( stdbuf -oL udevadm monitor --udev --kernel --subsystem-match=input 2>&1 | while IFS= read -r l; do printf "%s %s\n" "$(date -u +%H:%M:%S.%3N)" "$l"; done > target/udev-input.log ) &\
        udev_pid=$!;\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/debug/yserver > target/yserver-hotplug.log 2>&1 &\
        yserver_pid=$!;\
        for i in $(seq 100); do [ -S /tmp/.X11-unix/X7 ] && break; sleep 0.1; done;\
        DISPLAY=:7 fvwm3 > target/fvwm3-hotplug.log 2>&1 &\
        echo "up. NOW: power monitor OFF, wait ~5s, power ON, move mouse. Then zap/Ctrl-C.";\
        echo "logs: target/yserver-hotplug.log + target/udev-input.log";\
        wait $yserver_pid 2>/dev/null;\
        kill $udev_pid 2>/dev/null;'

# Bring up yserver ALONE on :7 (no compositor / no GL client), then run
# the GLX TFP probe as the FIRST and only client — so its dri3-screen /
# texture_from_pixmap result is representative of muffin's first-client
# position. Run from a free VT/tty on the HW box (needs DRM master).
# Output: probe result on stdout + /tmp/tfp-probe.out, yserver log in
# yserver-hw-bare.log. yserver is killed when the probe finishes.
yserver-tfp-probe-hw log="warn":
    cargo build --bin yserver
    gcc tools/glx-tfp-probe.c -lGL -lX11 -o ./tfp-probe
    bash -c '\
        xdg_rd=$(mktemp -d -t yserver-run.XXXXXX); chmod 700 "$xdg_rd";\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/debug/yserver > yserver-hw-bare.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        echo "=== GLX TFP probe (sole client) ===";\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:7 \
            XDG_RUNTIME_DIR="$xdg_rd" LIBGL_DEBUG=verbose \
            ./tfp-probe 2>&1 | tee tfp-probe.out \
            | grep -iE "screen|texture_from_pixmap|USABLE|matching|radeonsi|cfg [0-9]|returned";\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;\
        rm -rf "$xdg_rd" 2>/dev/null'

# Bring up yserver ALONE on :7 and run the XFIXES pointer-barrier smoke
# client (tools/barrier-smoke.c) as the SOLE client. With no args it puts
# a vertical barrier down the middle of the root window — on a symmetric
# dual-head box that IS the monitor seam. Push the physical mouse against
# the line: it should HOLD, BarrierHit events stream to stdout, and a firm
# push past {{release}} px of pressure auto-releases (pointer crosses). Set
# release=0 to test the pure trap (pointer never crosses). Pass explicit
# geometry via args="X1 Y1 X2 Y2" or args="--horizontal". Run from a TTY
# with DRM master. Diff stdout against the same binary under Xorg.
# Release gate for the pointer-barriers feature (audit T13).
yserver-barrier-smoke-hw log="info,yserver_core::barriers=trace" release="600" args="":
    cargo build --release --bin yserver
    gcc tools/barrier-smoke.c -lX11 -lXfixes -lXi -o ./barrier-smoke
    bash -c '\
        xdg_rd=$(mktemp -d -t yserver-run.XXXXXX); chmod 700 "$xdg_rd";\
        RUST_LOG="{{log}}" RUST_BACKTRACE=1 target/release/yserver > yserver-hw-barrier.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        echo "=== barrier smoke (sole client) — push the mouse at the line ===";\
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY=:7 \
            XDG_RUNTIME_DIR="$xdg_rd" RELEASE="{{release}}" \
            ./barrier-smoke {{args}} 2>&1 | tee barrier-smoke.out;\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null;\
        rm -rf "$xdg_rd" 2>/dev/null'

# DPMS / device-loss leak repro (the overnight "screen on but dead" bug,
# 2026-06-14). Launches yserver-hw with YSERVER_LOOP_TELEMETRY=1 (per-second
# PixmapPool stats — the leak detector) + targeted DPMS/scanout/device-loss
# logging, and stays alive so you can drive the workload and watch.
#
# Then, from an ssh shell, reproduce + observe:
#   # leak: does the PixmapPool count climb while the display is off?
#   tail -f target/yserver-telemetry.log | grep -iE 'pool|pixmap|DEVICE_LOST|dpms|disable_output'
#   # hotplug: does the connector drop the link on standby?
#   watch -n1 'cat /sys/class/drm/card1-HDMI-A-2/status /sys/class/drm/card1-HDMI-A-2/dpms'
#   dmesg -w | grep -iE 'hdmi|connector|hpd|amdgpu|reset'
# Drive it: start a continuous full-screen renderer (the real trigger was
# cinnamon-screensaver doing MIT-SHM PutImage), then `DISPLAY=:7 xset dpms
# force off` and leave it. Watch for pool growth + the device-loss cascade.
# Ctrl-C to stop.
yserver-dpms-telemetry log="info,yserver::kms::v2::backend=debug,yserver::kms::v2::platform=debug,yserver::kms::v2::scene=debug,yserver::kms::vk::scanout=debug,yserver::drm=debug":
    cargo build --bin yserver
    bash -c '\
        unset WAYLAND_DISPLAY WAYLAND_SOCKET;\
        export GDK_BACKEND=x11 XDG_SESSION_TYPE=x11;\
        RUST_LOG="{{log}}" YSERVER_LOOP_TELEMETRY=1 RUST_BACKTRACE=1 \
            target/debug/yserver > target/yserver-telemetry.log 2>&1 &\
        yserver_pid=$!;\
        sleep 2;\
        DISPLAY=:7 e16 > target/e16-dpms.log 2>&1 &\
        echo "yserver up on :7 (pid $yserver_pid); telemetry+log -> target/yserver-telemetry.log";\
        echo "drive: DISPLAY=:7 <full-screen renderer>, then DISPLAY=:7 xset dpms force off";\
        echo "watch: pool growth in the log; /sys/class/drm/.../status for hotplug. Ctrl-C to stop.";\
        wait $yserver_pid 2>/dev/null;'

# ============================== RENDERCHECK ==============================

# Run rendercheck (X RENDER smoke suite) against ynest on `display`.
# `tests` is a comma-separated list. Default budget is 600s/test —
# `composite` / `cacomposite` / `repeat` are intrinsically slow
# (massive operator × format × source enumeration). Set timeout=N to
# override.
rendercheck-ynest display="99" geometry="1024x768" timeout="600" tests="fill,dcoords,scoords,mcoords,tscoords,tmcoords,blend,composite,cacomposite,gradients,repeat,triangles,bug7366":
    cargo build --release --bin ynest
    DISPLAY=:0 RUST_LOG=warn target/release/ynest {{display}} --geometry {{geometry}} > ynest-rc.log 2>&1 & \
        pid=$!; \
        trap "kill $pid 2>/dev/null; wait" INT TERM EXIT; \
        sleep 1; \
        tools/rendercheck.sh :{{display}} {{timeout}} {{tests}}

# Run rendercheck against yserver (KMS) inside virtme-ng.
rendercheck-yserver timeout="600" tests="fill,dcoords,scoords,mcoords,tscoords,tmcoords,blend,composite,cacomposite,gradients,repeat,triangles,bug7366":
    cargo build --release --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-display egl-headless,gl=on -vga none -device virtio-vga-gl,hostmem=4G,blob=true,venus=true -device virtio-tablet-pci -device virtio-keyboard-pci" \
        -- tools/yserver-vng-run.sh rendercheck {{timeout}} {{tests}}

# Run rendercheck on host
rendercheck-yserver-hw timeout="60" tests="fill,dcoords,scoords,mcoords,tscoords,tmcoords,blend,composite,cacomposite,gradients,repeat,triangles,bug7366":
    tools/yserver-vng-run.sh rendercheck {{timeout}} {{tests}}

# ============================== XTS ==============================

# Boot ynest on `display` and run an xts5 scenario against it.
# `scenario` matches an entry in xts5/tet_scen (Xproto, Xlib3, …, all).
# Tally lands in xts/results/<timestamp>/summary.
xts-ynest scenario="Xproto" display="99" geometry="1024x768" timeout="600":
    cargo build --release --bin ynest
    DISPLAY=:0 RUST_LOG=warn target/release/ynest {{display}} --geometry {{geometry}} > ynest-xts.log 2>&1 & \
        pid=$!; \
        trap "kill $pid 2>/dev/null; wait" INT TERM EXIT; \
        sleep 1; \
        tools/xts-run.sh :{{display}} {{scenario}} {{timeout}}

# Run an xts5 scenario against yserver (KMS) inside virtme-ng.
# Boots vng once with yserver in the background (headless QEMU,
# virtio-gpu KMS), polls for the X socket on :7, then runs the same
# xts harness ynest uses. Result tree lands in xts/results/ on the
# host because vng mounts the host rootfs --rw.
# NOTE: uses the Venus GPU-passthrough display config (same as
# rendercheck), NOT `-device virtio-gpu-pci -display none`. The
# headless-no-display config leaves yserver's KMS pageflips with no
# completion event, which stalls the compose path and wedges clients
# drawing to windows — the draw-heavy scenarios (Xlib9) hung there.
# egl-headless gives a working display+flip path so the tests
# complete. Timeout is generous because GetImage-heavy verification
# runs slow under the guest's software/Venus Vulkan.
xts-yserver scenario="Xproto" timeout="1200":
    cargo build --release --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-display egl-headless,gl=on -vga none -device virtio-vga-gl,hostmem=4G,blob=true,venus=true -device virtio-tablet-pci -device virtio-keyboard-pci" \
        -- tools/yserver-vng-run.sh xts {{scenario}} {{timeout}}

xts-yserver-hw scenario="Xproto" timeout="1200":
    cargo build --release --bin yserver
    bash -c '\
        case "$(tty)" in /dev/tty[0-9]*) ;; *) echo "startx: must be run from a TTY (got: $(tty))" >&2; exit 1;; esac;\
        display=0;\
        while [ -e /tmp/.X11-unix/X$display ]; do display=$((display+1)); done;\
        echo "xts: using DISPLAY=:$display";\
        target/release/yserver "$display" > yserver-hw-xts.log 2>&1 &\
        yserver_pid=$!;\
        for i in $(seq 30); do [ -S /tmp/.X11-unix/X$display ] && break; sleep 1; done;\
        env DISPLAY=":$display" xset s off -dpms; \
        env DISPLAY=":$display" xterm -geometry 100x80-100+0 -e "tools/xts-run.sh :$display {{scenario}} {{timeout}}";\
        kill -TERM $yserver_pid 2>/dev/null;\
        wait $yserver_pid 2>/dev/null'
