#!/bin/bash
#
# tiko-postgres rootfs e2e: boot a VM from the tiko-postgres-rootfs.ext4
# image (patched Tiko PostgreSQL with S3 Files-backed storage, see
# scripts/rootfs/build_rootfs_tiko_postgres.sh), initialize and start the
# database inside the guest (init_pg.sh / start_pg.sh run as the postgres
# user), exercise it with a psql smoke test, then tear everything down —
# including the data the test wrote to the S3 Files mount.
#
# The test logic lives in clients/node/test-e2e/tiko_postgres.test.ts (driven
# through the official Node client, like test_node_client.sh); this wrapper
# only checks the image is present, compiles the test, starts hostd and runs
# it. Like test_s3files.sh this needs real AWS resources (the image only
# exists if someone built it from a filled-in s3files-config + tiko build
# outputs). Skip (pass) when the image has not been built.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=tests/common.sh
source "${SCRIPT_DIR}/common.sh"

if [[ ! -f "${REPO_ROOT}/assets/tiko-postgres-rootfs.ext4" ]]; then
	echo "assets/tiko-postgres-rootfs.ext4 not built (needs tiko build outputs + s3files-config) — skipping"
	exit 0
fi

trap 'cleanup_vms; stop_hostd' EXIT

NODE_CLIENT_DIR="${REPO_ROOT}/clients/node"

# Build the client library and compile the e2e test (Node's native type
# stripping does not rewrite .js -> .ts import specifiers).
(
	cd "${NODE_CLIENT_DIR}"
	if [[ ! -x node_modules/.bin/tsc ]]; then
		npm install --no-audit --no-fund
	fi
	npm run build:e2e
)

start_hostd

echo "Running tiko-postgres rootfs e2e against ${HOSTD_URL}"
(
	cd "${NODE_CLIENT_DIR}"
	TIKOVM_HOSTD_URL="${HOSTD_URL}" \
		TIKOVM_HOSTD_TOKEN="${HOSTD_TOKEN}" \
		TIKOVM_CREATED_VMS_FILE="${CREATED_VMS_FILE}" \
		node --test 'dist-e2e/test-e2e/tiko_postgres.test.js'
)

printf '\ntiko-postgres rootfs test passed. ✅\n\n'
