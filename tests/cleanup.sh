#!/bin/bash
# Cleanup script for i3mux test containers

DOCKER_CMD="podman"
if ! command -v podman &> /dev/null; then
    DOCKER_CMD="docker"
fi

echo "Cleaning up i3mux test containers..."

# Stop and remove all test containers
$DOCKER_CMD rm -f \
    $($DOCKER_CMD ps -aq --filter "ancestor=localhost/docker_i3mux-test-xephyr:latest") \
    $($DOCKER_CMD ps -aq --filter "ancestor=localhost/docker_i3mux-remote-ssh:latest") \
    2>/dev/null

# Also clean by name pattern
$DOCKER_CMD rm -f $($DOCKER_CMD ps -aq --filter "name=i3mux-test") 2>/dev/null
$DOCKER_CMD rm -f $($DOCKER_CMD ps -aq --filter "name=i3mux-remote") 2>/dev/null

echo "✓ Cleanup complete"
$DOCKER_CMD ps -a | grep -E "i3mux|docker_i3mux" || echo "  No i3mux containers remaining"
