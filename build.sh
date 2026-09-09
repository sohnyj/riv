#!/bin/sh
set -e
cd "$(dirname "$0")"
. "$HOME/.cargo/env"
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo build --release
CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUNNER="${CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUNNER:-wine}" WINEDEBUG="${WINEDEBUG:--all}" cargo test
