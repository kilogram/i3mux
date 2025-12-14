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

# Set terminal title
printf '\033]0;%s\007' "$TITLE"

# Reset terminal modes for proper scrollback
printf '\033[?1l'    # Disable application cursor keys
printf '\033[?1000l' # Disable mouse tracking
printf '\033[?1002l' # Disable cell motion mouse tracking
printf '\033[?1006l' # Disable SGR mouse mode

echo "[i3mux wrapper] Running attach command..."

# Run the attach command
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
