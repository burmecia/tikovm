#!/bin/bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# Point vmtop at the same local hostd used by the e2e scripts. `TIKOVM_HOSTD_API_TOKEN`
# is optional here — pass `--token` or export it to talk to a real hostd.
export TIKOVM_HOSTD_API_TOKEN="${TIKOVM_HOSTD_API_TOKEN:-xxx}"

# Build as the current user; vmtop is a plain HTTP client, no root needed.
cargo build --manifest-path "${SCRIPT_DIR}/../Cargo.toml" -p vmtop

exec "${SCRIPT_DIR}/../target/debug/vmtop" "$@"