#!/bin/bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

export RUST_LOG=hostd=debug,tower_http=debug
export TIKOVM_HOSTD_API_TOKEN=xxx
export FIRECRACKER_BIN="${HOME}/firecracker/build/cargo_target/x86_64-unknown-linux-musl/debug/firecracker"

cargo run --manifest-path "${SCRIPT_DIR}/../Cargo.toml" -p hostd -- \
    --assets-dir "${SCRIPT_DIR}/../assets"