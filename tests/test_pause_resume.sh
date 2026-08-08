#!/bin/bash
#
# Pause/resume: the hostd API state and the Firecracker-reported state must
# agree after each operation, and resuming a running VM must fail with the
# uniform JSON error.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=tests/common.sh
source "${SCRIPT_DIR}/common.sh"

trap 'cleanup_vms; stop_hostd' EXIT

start_hostd

VM_ID="$(create_vm "e2e-vm")"
echo "Created VM: ${VM_ID}"

# Pause the VM, expecting the API and Firecracker to agree on the paused state
PAUSE_RESPONSE="$(api_post "/api/vms/${VM_ID}/pause")"
echo "Pause response: ${PAUSE_RESPONSE}"

PAUSE_STATUS="$(jq -r '.state' <<<"${PAUSE_RESPONSE}")"
if [[ "${PAUSE_STATUS}" != "paused" ]]; then
	echo "Unexpected pause response: ${PAUSE_RESPONSE}"
	exit 1
fi

expect_fc_state "${VM_ID}" "Paused"
echo "VM ${VM_ID} paused (Firecracker agrees)"

# Resume the VM, expecting it to end up back in the started/running state
RESUME_RESPONSE="$(api_post "/api/vms/${VM_ID}/resume")"
echo "Resume response: ${RESUME_RESPONSE}"

RESUME_STATUS="$(jq -r '.state' <<<"${RESUME_RESPONSE}")"
if [[ "${RESUME_STATUS}" != "started" ]]; then
	echo "Unexpected resume response: ${RESUME_RESPONSE}"
	exit 1
fi

expect_fc_state "${VM_ID}" "Running"
echo "VM ${VM_ID} resumed (Firecracker reports Running)"

# Resuming a VM that is not paused must fail with the uniform JSON error
RESUME_AGAIN_RAW="$(api_raw POST "/api/vms/${VM_ID}/resume")"
expect_error_code "${RESUME_AGAIN_RAW}" "409"
echo "Second resume returned expected error: $(api_body "${RESUME_AGAIN_RAW}")"

printf '\nPause/resume test passed. ✅\n\n'
