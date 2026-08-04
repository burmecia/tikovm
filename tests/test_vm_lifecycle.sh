#!/bin/bash
#
# VM lifecycle: create -> get -> boot -> list -> delete, plus the uniform 404
# JSON errors after delete and the work-dir cleanup.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=tests/common.sh
source "${SCRIPT_DIR}/common.sh"

trap 'cleanup_vms; stop_hostd' EXIT

start_hostd

# Create a VM using the hostd API (the create endpoint also boots the VM, so
# the stable state is "started", not "created").
VM_ID="$(create_vm "e2e-vm")"
echo "Created VM: ${VM_ID}"

# Get the VM, expecting its id, state, and config to round-trip.
GET_RESPONSE="$(api_get "/api/vms/${VM_ID}")"
echo "Get response: ${GET_RESPONSE}"

GET_ID="$(jq -r '.vm_id' <<<"${GET_RESPONSE}")"
GET_STATUS="$(jq -r '.state' <<<"${GET_RESPONSE}")"
GET_NAME="$(jq -r '.vm_config.name' <<<"${GET_RESPONSE}")"
if [[ "${GET_ID}" != "${VM_ID}" || "${GET_STATUS}" != "started" || "${GET_NAME}" != "e2e-vm" ]]; then
	echo "Unexpected get response: ${GET_RESPONSE}"
	exit 1
fi
echo "Got VM ${VM_ID} (status: ${GET_STATUS})"

# hostd waits for Firecracker to report the VM as "Running" before it marks
# the VM as "started", so the Firecracker API on the VM's socket must agree.
expect_fc_state "${VM_ID}" "Running"
echo "Firecracker reports VM ${VM_ID} as Running"

# Verify the VM actually boots, watching the serial console for a login
# prompt (or a rescue shell, which means the overlay setup failed).
wait_serial_boot "${VM_ID}"
echo "VM ${VM_ID} booted to a login prompt"

# List VMs, expecting the created VM to be present
LIST_RESPONSE="$(api_get /api/vms)"
echo "List response: ${LIST_RESPONSE}"

LIST_COUNT="$(jq --arg id "${VM_ID}" '[.[] | select(.vm_id == $id)] | length' <<<"${LIST_RESPONSE}")"
if [[ "${LIST_COUNT}" != "1" ]]; then
	echo "Expected VM ${VM_ID} in list response: ${LIST_RESPONSE}"
	exit 1
fi
LIST_STATUS="$(jq -r --arg id "${VM_ID}" '.[] | select(.vm_id == $id) | .state' <<<"${LIST_RESPONSE}")"
LIST_NAME="$(jq -r --arg id "${VM_ID}" '.[] | select(.vm_id == $id) | .vm_config.name' <<<"${LIST_RESPONSE}")"
if [[ "${LIST_STATUS}" != "started" || "${LIST_NAME}" != "e2e-vm" ]]; then
	echo "Unexpected list entry: ${LIST_RESPONSE}"
	exit 1
fi
echo "VM list contains ${VM_ID} (status: ${LIST_STATUS})"

# Delete the VM, expecting 204 No Content
DELETE_RAW="$(api_raw DELETE "/api/vms/${VM_ID}")"
DELETE_CODE="$(api_code "${DELETE_RAW}")"
if [[ "${DELETE_CODE}" != "204" ]]; then
	echo "Expected 204 from delete, got ${DELETE_CODE}"
	exit 1
fi
echo "Deleted VM ${VM_ID} (HTTP ${DELETE_CODE})"

# Getting the deleted VM should fail with a uniform 404 JSON error
GET_DELETED_RAW="$(api_raw GET "/api/vms/${VM_ID}")"
expect_error_code "${GET_DELETED_RAW}" "404"
echo "Get after delete returned expected 404 JSON error: $(api_body "${GET_DELETED_RAW}")"

# The VM's whole work dir (socket, logs, overlay disk, snapshot files)
# should be cleaned up
if [[ -e "/tmp/tikovm/${VM_ID}" ]]; then
	echo "VM work dir /tmp/tikovm/${VM_ID} was not cleaned up"
	exit 1
fi

# Deleting the same VM again should fail with a uniform 404 JSON error
DELETE_AGAIN_RAW="$(api_raw DELETE "/api/vms/${VM_ID}")"
expect_error_code "${DELETE_AGAIN_RAW}" "404"
DELETE_AGAIN_BODY="$(api_body "${DELETE_AGAIN_RAW}")"
ERROR_MESSAGE="$(jq -r '.error.message' <<<"${DELETE_AGAIN_BODY}")"
if [[ -z "${ERROR_MESSAGE}" || "${ERROR_MESSAGE}" == "null" ]]; then
	echo "Unexpected error body: ${DELETE_AGAIN_BODY}"
	exit 1
fi
echo "Second delete returned expected 404 JSON error: ${DELETE_AGAIN_BODY}"

# The deleted VM should no longer be listed
LIST_AFTER_DELETE="$(api_get /api/vms)"
LIST_AFTER_DELETE_COUNT="$(jq 'length' <<<"${LIST_AFTER_DELETE}")"
if [[ "${LIST_AFTER_DELETE_COUNT}" != "0" ]]; then
	echo "Expected empty VM list after delete, got: ${LIST_AFTER_DELETE}"
	exit 1
fi
echo "VM list is empty after delete"

printf '\nVM lifecycle test passed. ✅\n\n'
