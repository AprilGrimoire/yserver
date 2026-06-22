#!/bin/sh
# #48: pre-start xfconfd on the (fresh) session bus before launching the
# xfce session, so clients (thunar/xfdesktop) don't each pay the ~10s dbus
# on-demand activation that the bare `dbus-run-session` harness incurs
# (xfconfd's own init is ~9ms; the 10s lives entirely in the activation
# handshake, so starting it directly bypasses the cost rather than
# relocating it). MUST be run inside `dbus-run-session`. Execs the session
# command passed as arguments, e.g.:
#   dbus-run-session sh tools/xfce-session-prewarm.sh xfce4-session --display :7
#
# Path-robust: take the binary from the authoritative dbus .service
# `Exec=` (correct on any distro layout — Arch /usr/lib*, Debian/PikaOS
# multiarch /usr/lib/<triplet> or /usr/libexec), with a glob fallback.

start_xfconfd() {
    for dir in /usr/share/dbus-1/services \
               /usr/local/share/dbus-1/services \
               "${XDG_DATA_HOME:-$HOME/.local/share}/dbus-1/services"; do
        svc="$dir/org.xfce.Xfconf.service"
        [ -r "$svc" ] || continue
        exec_line=$(sed -n 's/^Exec=//p' "$svc" | head -1)
        if [ -n "$exec_line" ]; then
            # shellcheck disable=SC2086  # Exec= may carry args; want word-split
            $exec_line >/dev/null 2>&1 &
            return 0
        fi
    done
    # Fallback: glob the common install locations.
    for p in /usr/lib*/xfce4/xfconf/xfconfd \
             /usr/lib/*/xfce4/xfconf/xfconfd \
             /usr/libexec/xfce4/xfconf/xfconfd; do
        if [ -x "$p" ]; then
            "$p" >/dev/null 2>&1 &
            return 0
        fi
    done
    echo "xfce-session-prewarm: xfconfd not found — clients may stall on dbus activation" >&2
    return 1
}

start_xfconfd
exec "$@"
