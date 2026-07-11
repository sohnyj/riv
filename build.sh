#!/bin/sh
# Build entry point: fmt, clippy (-D warnings), then build.
set -e
cd "$(dirname "$0")"
. "$HOME/.cargo/env"
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo build "$@"
