#!/usr/bin/env bash
# i3mux wrapper script - runs locally to launch terminals with proper setup
# This handles logging, terminal mode setup, and cleanup

set -euo pipefail

# Parse arguments
SOCKET="$1"
TITLE="$2"
ATTACH_CMD="$3"
CLEANUP_CMD="$4"
PROMPT_CMD="${5:-}"  # Optional

LOG_FILE="/tmp/i3mux-${SOCKET}.log"

# Set terminal title BEFORE redirecting output (must go to actual terminal)
printf '\033]0;%s\007' "$TITLE"

# Redirect all output to log file
exec &> >(tee -a "$LOG_FILE") 2>&1

echo "[i3mux wrapper] Starting at $(date)"
echo "[i3mux wrapper] Title: $TITLE"
echo "[i3mux wrapper] Socket: $SOCKET"
echo "[i3mux wrapper] Attach command: $ATTACH_CMD"

# Set PROMPT_COMMAND if provided (for maintaining title)
if [ -n "$PROMPT_CMD" ]; then
    export PROMPT_COMMAND="$PROMPT_CMD"
fi

echo "[i3mux wrapper] Running attach command..."

# Run the attach command
# Note: SIGWINCH should propagate automatically to the foreground process
eval "$ATTACH_CMD"
RC=$?

echo "[i3mux wrapper] Attach command exited with code: $RC"

# Run cleanup if provided
if [ -n "$CLEANUP_CMD" ]; then
    eval "$CLEANUP_CMD"
fi

echo "[i3mux wrapper] Session ended at $(date)"

# Only pause on error exit codes
if [ $RC -ne 0 ]; then
    read -p "Press Enter to close terminal..." || true
fi
