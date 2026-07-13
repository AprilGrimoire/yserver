#!/usr/bin/env bash
set -euo pipefail

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)

cd "$repo_root"

kernel=${KERNEL:-/boot/vmlinuz-linux-cachyos}
timeout_seconds=${1:-600}
trace_drawable_arg=${2:-}
log_path=${YSERVER_VNG_LOG:-$repo_root/yserver-vng-xfill.log}
submit_trace_path=${YSERVER_SUBMIT_TRACE:-$repo_root/yserver-vng-xfill.submit.tsv}
fb_trace_drawable_id=${YSERVER_FB_TRACE_DRAWABLE_ID:-${trace_drawable_arg:-429}}
rust_log=${YSERVER_VNG_RUST_LOG:-warn,yserver::kms::render::fbtrace=warn,yserver::kms::render::fill=trace}

rm -f "$log_path" "$submit_trace_path"

RUSTC_WRAPPER= cargo build --release --bin yserver

exec vng -r "$kernel" --disable-microvm --rw \
    --qemu-opts="-display egl-headless,gl=on -vga none -device virtio-vga-gl,hostmem=4G,blob=true,venus=true -device virtio-tablet-pci -device virtio-keyboard-pci" \
    -- env \
    YSERVER_VNG_LOG="$log_path" \
    YSERVER_VNG_RUST_LOG="$rust_log" \
    YSERVER_SUBMIT_TRACE="$submit_trace_path" \
    YSERVER_FB_TRACE_DRAWABLE_ID="$fb_trace_drawable_id" \
    tools/yserver-vng-run.sh xts XFillRectangle "$timeout_seconds"
