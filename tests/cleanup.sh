#!/usr/bin/env bash
# Remove every i3mux test container (compose-started or testcontainers-started).
set -euo pipefail

IMAGE="i3mux-test-sway:latest"

if command -v podman >/dev/null 2>&1; then
    cli=podman
else
    cli=docker
fi

mapfile -t ids < <(
    {
        "$cli" ps -aq --filter "ancestor=$IMAGE"
        "$cli" ps -aq --filter "ancestor=localhost/$IMAGE"
        "$cli" ps -aq --filter "name=i3mux-test"
        "$cli" ps -aq --filter "name=i3mux-remote"
    } 2>/dev/null | sort -u
)

if ((${#ids[@]} > 0)); then
    "$cli" rm -f "${ids[@]}" >/dev/null
    echo "Removed ${#ids[@]} i3mux test container(s)."
else
    echo "No i3mux test containers found."
fi
