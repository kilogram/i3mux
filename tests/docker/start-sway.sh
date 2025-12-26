#!/bin/bash
set -e

echo "Starting Sway in headless mode..."

# Ensure XDG_RUNTIME_DIR exists
export XDG_RUNTIME_DIR=/tmp/runtime-root
mkdir -p $XDG_RUNTIME_DIR
chmod 700 $XDG_RUNTIME_DIR

# Configure wlroots for headless operation with software rendering
export WLR_BACKENDS=headless
export WLR_LIBINPUT_NO_DEVICES=1
export WLR_RENDERER=pixman
export LIBGL_ALWAYS_SOFTWARE=1

# Start Sway with test config
echo "Starting Sway with test config..."
sway -c /opt/i3mux-test/sway-test-config 2>&1 &
SWAY_PID=$!
echo "Sway started with PID $SWAY_PID"

# Wait for Sway socket to appear
echo "Waiting for Sway socket..."
TIMEOUT=10
for i in $(seq 1 $TIMEOUT); do
    # Find the Sway socket - check both /tmp and XDG_RUNTIME_DIR
    SOCKET=$(ls $XDG_RUNTIME_DIR/sway-ipc.*.sock 2>/dev/null | head -1 || ls /tmp/sway-ipc.*.sock 2>/dev/null | head -1 || true)
    if [ -n "$SOCKET" ] && [ -S "$SOCKET" ]; then
        export SWAYSOCK="$SOCKET"
        echo "Sway socket found: $SWAYSOCK"
        break
    fi
    if [ $i -eq $TIMEOUT ]; then
        echo "ERROR: Sway failed to create socket within ${TIMEOUT} seconds"
        exit 1
    fi
    sleep 1
done

# Verify Sway is running
if swaymsg -t get_version >/dev/null 2>&1; then
    echo "Sway is ready!"
    swaymsg workspace 1
else
    echo "ERROR: Sway failed to start properly"
    exit 1
fi

# Find the wayland display (usually wayland-1 or wayland-0)
WAYLAND_DISPLAY=$(ls $XDG_RUNTIME_DIR/wayland-* 2>/dev/null | head -1 | xargs basename || echo "wayland-1")
export WAYLAND_DISPLAY

# Export environment for child processes
cat > /tmp/sway-env.sh << EOF
export SWAYSOCK=$SWAYSOCK
export XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR
export WAYLAND_DISPLAY=$WAYLAND_DISPLAY
EOF

echo "Test environment is ready!"
echo "  - Sway running in headless mode"
echo "  - Socket: $SWAYSOCK"
echo "  - Current workspace: 1"

# Keep container running
wait $SWAY_PID
