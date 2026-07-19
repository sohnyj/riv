#!/bin/sh
# Build entry point: fmt, clippy (-D warnings), build, then tests under wine.
set -e
cd "$(dirname "$0")"
. "$HOME/.cargo/env"
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo build "$@"
CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUNNER="${CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUNNER:-wine}" cargo test
