#!/bin/bash
set -e

cargo build --target x86_64-unknown-linux-musl
cargo test --test integration "$@"
