#!/usr/bin/env bash
# i3mux remote helper script
# This script is uploaded to remote hosts to handle i3mux operations reliably
# with proper environment setup (PATH, etc.)

set -euo pipefail

VERSION="1.0.4"

# Check if abduco is available (sources login profile for PATH)
cmd_check_deps() {
    if ! command -v abduco &>/dev/null; then
        echo "ERROR: abduco not found" >&2
        echo "Install abduco on this host:" >&2
        echo "  - Arch Linux: sudo pacman -S abduco" >&2
        echo "  - Debian/Ubuntu: sudo apt install abduco" >&2
        echo "  - Or build from source: https://github.com/martanne/abduco" >&2
        exit 1
    fi
    # Output path to abduco for verification
    command -v abduco
}

# Attach to an abduco session (runs specified command or user's shell)
# Usage: attach <socket> [-- <cmd>]
cmd_attach() {
    local socket="$1"
    shift

    # Check for -- separator
    if [[ "${1:-}" == "--" ]]; then
        shift
        # Run the specified command in abduco
        exec abduco -A "/tmp/$socket" "$@"
    else
        # Default: run user's shell
        exec abduco -A "/tmp/$socket" "$SHELL"
    fi
}

# Check if any abduco sessions exist for a workspace prefix, clean up if none
cmd_cleanup_check() {
    local ws_prefix="$1"
    local session="$2"

    # Check if any socket files with this prefix exist in /tmp/
    # (abduco sessions create socket files in /tmp/)
    if ls /tmp/${ws_prefix}-* &>/dev/null; then
        # Sessions still exist, don't clean up
        exit 0
    else
        # No sessions exist, safe to clean up session files
        rm -f "/tmp/i3mux/sessions/${session}.json"
        rm -f "/tmp/i3mux/locks/${session}.lock"
        exit 0
    fi
}

# Output version for script update detection
cmd_version() {
    echo "$VERSION"
}

# Main command dispatcher
case "${1:-}" in
    check-deps)
        cmd_check_deps
        ;;
    attach)
        shift
        cmd_attach "$@"
        ;;
    cleanup-check)
        shift
        cmd_cleanup_check "$@"
        ;;
    version)
        cmd_version
        ;;
    *)
        echo "Usage: $0 {check-deps|attach|cleanup-check|version}" >&2
        exit 1
        ;;
esac
