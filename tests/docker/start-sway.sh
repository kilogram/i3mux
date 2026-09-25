#!/usr/bin/env bash
# Start headless Sway for the integration tests. Writes /tmp/sway-env.sh
# for `podman exec` callers and prints "Test environment is ready!" when done.
set -euo pipefail
shopt -s nullglob

export XDG_RUNTIME_DIR=/tmp/runtime-root
export WLR_BACKENDS=headless
export WLR_LIBINPUT_NO_DEVICES=1
export WLR_RENDERER=pixman
export LIBGL_ALWAYS_SOFTWARE=1
mkdir -p "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"

echo "Starting Sway with test config..."
sway -c /opt/i3mux-test/sway-test-config 2>&1 &
SWAY_PID=$!

TIMEOUT=10
SWAYSOCK=""
for ((i = 1; i <= TIMEOUT; i++)); do
    for sock in "$XDG_RUNTIME_DIR"/sway-ipc.*.sock; do
        if [[ -S "$sock" ]]; then
            SWAYSOCK="$sock"
            break 2
        fi
    done
    sleep 1
done
if [[ -z "$SWAYSOCK" ]]; then
    echo "ERROR: Sway did not create its IPC socket within ${TIMEOUT}s" >&2
    exit 1
fi
export SWAYSOCK

if ! swaymsg -t get_version >/dev/null 2>&1; then
    echo "ERROR: Sway IPC socket $SWAYSOCK does not answer" >&2
    exit 1
fi
swaymsg workspace 1 >/dev/null

WAYLAND_DISPLAY=wayland-1
for disp in "$XDG_RUNTIME_DIR"/wayland-*; do
    [[ -S "$disp" ]] && { WAYLAND_DISPLAY=$(basename "$disp"); break; }
done
export WAYLAND_DISPLAY

cat > /tmp/sway-env.sh <<ENV
export SWAYSOCK=$SWAYSOCK
export XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR
export WAYLAND_DISPLAY=$WAYLAND_DISPLAY
ENV

echo "Test environment is ready!"
echo "  - Socket: $SWAYSOCK"
echo "  - Display: $WAYLAND_DISPLAY"

wait "$SWAY_PID"
