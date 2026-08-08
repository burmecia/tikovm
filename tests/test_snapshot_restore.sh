#!/bin/bash
#
# Snapshot/restore: a snapshotted VM ends up suspended with its Firecracker
# process stopped and only the snapshot files remaining; restore brings it
# back to started/running. Repeated snapshot/restore calls on a VM in the
# wrong state must fail with the uniform JSON error.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=tests/common.sh
source "${SCRIPT_DIR}/common.sh"

trap 'cleanup_vms; stop_hostd' EXIT

start_hostd

VM_ID="$(create_vm "e2e-vm")"
echo "Created VM: ${VM_ID}"

FC_SOCKET="/tmp/tikovm/${VM_ID}/${VM_ID}.socket"

# Snapshot the running VM, expecting it to end up suspended with the
# snapshot files written to the work dir
SNAP_RESPONSE="$(api_post "/api/vms/${VM_ID}/snapshot")"
echo "Snapshot response: ${SNAP_RESPONSE}"

SNAP_STATE_PATH="$(jq -r '.state_path' <<<"${SNAP_RESPONSE}")"
SNAP_MEM_PATH="$(jq -r '.mem_path' <<<"${SNAP_RESPONSE}")"
if [[ ! -s "${SNAP_STATE_PATH}" || ! -s "${SNAP_MEM_PATH}" ]]; then
	echo "Snapshot files missing or empty: ${SNAP_STATE_PATH} ${SNAP_MEM_PATH}"
	exit 1
fi

GET_RESPONSE="$(api_get "/api/vms/${VM_ID}")"
SNAP_STATUS="$(jq -r '.state' <<<"${GET_RESPONSE}")"
if [[ "${SNAP_STATUS}" != "suspended" ]]; then
	echo "Expected state 'suspended' after snapshot, got: ${GET_RESPONSE}"
	exit 1
fi

# A suspended VM must not consume resources: the Firecracker process is
# stopped and its API socket removed; only the snapshot files remain.
if [[ -e "${FC_SOCKET}" ]]; then
	echo "Firecracker socket ${FC_SOCKET} still exists for a suspended VM"
	exit 1
fi
if pgrep -f "firecracker .*--id ${VM_ID}" >/dev/null 2>&1; then
	echo "Firecracker process for ${VM_ID} is still running after snapshot"
	exit 1
fi
echo "VM ${VM_ID} snapshotted and suspended (Firecracker process stopped)"

# Snapshotting an already-suspended VM must fail with the uniform JSON error
SNAP_AGAIN_RAW="$(api_raw POST "/api/vms/${VM_ID}/snapshot")"
expect_error_code "${SNAP_AGAIN_RAW}" "409"
echo "Second snapshot returned expected error: $(api_body "${SNAP_AGAIN_RAW}")"

# Restore the VM from its snapshot, expecting it back in started/running
RESTORE_RESPONSE="$(api_post "/api/vms/${VM_ID}/restore")"
echo "Restore response: ${RESTORE_RESPONSE}"

RESTORE_STATUS="$(jq -r '.state' <<<"${RESTORE_RESPONSE}")"
if [[ "${RESTORE_STATUS}" != "started" ]]; then
	echo "Unexpected restore response: ${RESTORE_RESPONSE}"
	exit 1
fi

expect_fc_state "${VM_ID}" "Running"
echo "VM ${VM_ID} restored from snapshot (Firecracker reports Running)"

# Restoring a VM that is not suspended must fail with the uniform JSON error
RESTORE_AGAIN_RAW="$(api_raw POST "/api/vms/${VM_ID}/restore")"
expect_error_code "${RESTORE_AGAIN_RAW}" "409"
echo "Second restore returned expected error: $(api_body "${RESTORE_AGAIN_RAW}")"

printf '\nSnapshot/restore test passed. ✅\n\n'
