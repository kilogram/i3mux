# i3mux task runner. CI calls these recipes and nothing else.
set shell := ["bash", "-euo", "pipefail", "-c"]

musl_target := "x86_64-unknown-linux-musl"
# podman-compose locally; CI sets COMPOSE="docker compose".
compose := env("COMPOSE", "podman-compose")

default: check

# Format, lint, boundary rules, unit tests (what the CI unit job runs)
check: check-boundaries
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    cargo test --bins

# Needs $SHPOOL_BIN or shpool on PATH; fails (never skips) without it.
# pty tests of the generated shpool attach/detach against a real shpool
check-mux:
    cargo test --bins mux::pty_tests -- --ignored

# Plan rule 8: layout.rs and window.rs are pure over sway JSON and never import wm.rs
check-boundaries:
    #!/usr/bin/env bash
    set -euo pipefail
    # Matches `crate::wm`, `super::wm`, and `crate::{.., wm, ..}` on one line.
    if offenders=$(grep -nE '\b(crate|super)::(\{[^}]*\b)?wm\b' src/layout.rs src/window.rs); then
        echo "$offenders" | sed 's/^/boundary violation: /' >&2
        echo "rule: src/layout.rs and src/window.rs must not import crate::wm (docs/PLAN-sway-shpool.md, rule 8)." >&2
        echo "fix: pass sway JSON in from the caller instead." >&2
        exit 1
    fi

# Container integration suite; args go to `cargo test`
check-int *args: images
    tests/integration.sh {{args}}

# Static binary that the test containers run
build-musl:
    cargo build --target {{musl_target}}

# Build the headless-sway + sshd test image (i3mux-test-sway:latest)
images:
    cd tests/docker && {{compose}} build i3mux-test-sway

# Regenerate layout golden images; review the diff before committing
update-goldens *args: images
    UPDATE_GOLDENS=1 tests/integration.sh {{args}}

# Remove leftover test containers
clean-containers:
    tests/cleanup.sh
