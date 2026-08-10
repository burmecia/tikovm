#!/bin/bash
#
# Node.js client e2e: exercise the official TypeScript client library
# (clients/node, npm package "tikovm") against a real hostd instance. The
# node test drives the whole VM lifecycle through the client (create with
# defaults, boot, list/get, pause/resume, snapshot/restore, exec, error
# mapping, delete); this wrapper only starts hostd and reports the result.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=tests/common.sh
source "${SCRIPT_DIR}/common.sh"

trap 'cleanup_vms; stop_hostd' EXIT

NODE_CLIENT_DIR="${REPO_ROOT}/clients/node"

# Build the client library and compile the e2e test. Node's native type
# stripping does not rewrite .js -> .ts import specifiers, so the test is
# compiled to dist-e2e/ before running (tsconfig.e2e.json).
(
	cd "${NODE_CLIENT_DIR}"
	if [[ ! -x node_modules/.bin/tsc ]]; then
		npm install --no-audit --no-fund
	fi
	npm run build:e2e
)

start_hostd

echo "Running node client e2e against ${HOSTD_URL}"
(
	cd "${NODE_CLIENT_DIR}"
	TIKOVM_HOSTD_URL="${HOSTD_URL}" \
		TIKOVM_HOSTD_TOKEN="${HOSTD_TOKEN}" \
		TIKOVM_CREATED_VMS_FILE="${CREATED_VMS_FILE}" \
		node --test 'dist-e2e/test-e2e/*.test.js'
)

printf '\nNode client test passed. ✅\n\n'
