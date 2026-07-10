#!/bin/sh
# 빌드 진입점 — P12: 매 빌드 fmt → clippy(-D warnings) → build
set -e
cd "$(dirname "$0")"
. "$HOME/.cargo/env"
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo build "$@"
