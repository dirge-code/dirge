#!/bin/sh
# Start Xvfb and openbox, then execute the requested command.
# This gives firefox and xdotool a virtual display to work with.

set -e

XVFB_PID=""
BOX_PID=""

cleanup() {
    [ -n "$BOX_PID" ] && kill "$BOX_PID" 2>/dev/null || true
    [ -n "$XVFB_PID" ] && kill "$XVFB_PID" 2>/dev/null || true
    exit 0
}
trap cleanup INT TERM

# Start virtual framebuffer
Xvfb :99 -screen 0 1920x1080x24 -ac &
XVFB_PID=$!
sleep 1

# Verify Xvfb is running
if ! kill -0 "$XVFB_PID" 2>/dev/null; then
    echo "ERROR: Xvfb failed to start" >&2
    exit 1
fi

# Start a minimal window manager (firefox needs one)
openbox --replace &
BOX_PID=$!
sleep 1

echo "==> Xvfb running on :99 (1920x1080)"
echo "==> openbox window manager started"
echo "==> DISPLAY=:99 is set"
echo "==> Ready for computer-use testing"
echo ""

# Run the requested command (default: /bin/bash)
exec "$@"
