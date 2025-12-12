#!/bin/bash
set -e

echo "Starting Xvfb on display :99..."

# Start Xvfb (virtual framebuffer) in background
Xvfb :99 \
    -screen 0 1920x1080x24 \
    -ac \
    +extension GLX \
    +render \
    -noreset \
    2>&1 &

XVFB_PID=$!
echo "Xvfb started with PID $XVFB_PID"

# Set display for subsequent commands
export DISPLAY=:99

# Wait for X server to be ready
echo "Waiting for X server to be ready..."
TIMEOUT=10
for i in $(seq 1 $TIMEOUT); do
    if xdpyinfo >/dev/null 2>&1; then
        echo "X server is ready!"
        break
    fi
    if [ $i -eq $TIMEOUT ]; then
        echo "ERROR: X server failed to start within ${TIMEOUT} seconds"
        exit 1
    fi
    sleep 1
done

# Create X authority file
touch /tmp/.Xauthority
chmod 600 /tmp/.Xauthority
export XAUTHORITY=/tmp/.Xauthority

# Start i3 with test config
echo "Starting i3 window manager..."
i3 -c /opt/i3mux-test/i3-test-config 2>&1 &
I3_PID=$!
echo "i3 started with PID $I3_PID"

# Wait for i3 to be ready
echo "Waiting for i3 to be ready..."
sleep 2

# Verify i3 is running
if i3-msg -t get_version >/dev/null 2>&1; then
    echo "i3 is ready!"
    i3-msg workspace 1
else
    echo "ERROR: i3 failed to start properly"
    exit 1
fi

echo "Test environment is ready!"
echo "  - Xvfb running on :99"
echo "  - i3 window manager active"
echo "  - Current workspace: 1"

# Keep container running and monitor processes
wait $XVFB_PID $I3_PID
