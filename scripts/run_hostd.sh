#!/bin/bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

export RUST_LOG=hostd=debug,tower_http=debug
export TIKOVM_HOSTD_API_TOKEN=xxx
export FIRECRACKER_BIN="${HOME}/firecracker/build/cargo_target/x86_64-unknown-linux-musl/debug/firecracker"

# Build as the current user so target/ stays user-writable; run the binary as
# root because hostd manages bridges/TAPs, iptables NAT rules and loop-mounts
# overlay disks.
cargo build --manifest-path "${SCRIPT_DIR}/../Cargo.toml" -p hostd

exec sudo -E "${SCRIPT_DIR}/../target/debug/hostd" \
    --assets-dir "${SCRIPT_DIR}/../assets"
