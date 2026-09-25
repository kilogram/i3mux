#!/usr/bin/env bash
# Build the static i3mux binary that the containers run, then run the
# container integration suite. Extra args go to `cargo test`.
#   tests/integration.sh                   # whole suite
#   tests/integration.sh layout -- --nocapture
set -euo pipefail

cd "$(dirname "$0")/.."
cargo build --target x86_64-unknown-linux-musl
cargo test --test integration "$@"
