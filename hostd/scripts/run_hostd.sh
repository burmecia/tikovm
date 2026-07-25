#!/bin/bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

export RUST_LOG=hostd=debug,tower_http=debug
export TIKOVM_HOSTD_API_TOKEN=xxx

cargo run --manifest-path "${SCRIPT_DIR}/../Cargo.toml" -p hostd