#!/bin/bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

export FIRECRACKER_BIN="${HOME}/firecracker/build/cargo_target/x86_64-unknown-linux-musl/debug/firecracker"

HOSTD_ADDR="127.0.0.1:3000"
HOSTD_URL="http://${HOSTD_ADDR}"
HOSTD_PORT="${HOSTD_ADDR##*:}"
HOSTD_TOKEN="${TIKOVM_HOSTD_API_TOKEN:-xxx}"
LOG_FILE="$(mktemp -t tikovm-hostd-e2e.XXXXXX.log)"

cleanup() {
	if [[ -n "${HOSTD_PID:-}" ]]; then
		# HOSTD_PID is a process group leader (started via setsid), so kill the
		# whole group to make sure cargo and the hostd binary it spawns are
		# both terminated instead of being left behind as orphans.
		kill -TERM -- "-${HOSTD_PID}" >/dev/null 2>&1 || true
		wait "${HOSTD_PID}" >/dev/null 2>&1 || true
	fi
}

trap cleanup EXIT

# Make sure no stale process (e.g. left over from a previous manual run) is
# already bound to the port, otherwise the readiness check below could pass
# against that stale server instead of the instance this script starts.
if fuser -n tcp "${HOSTD_PORT}" >/dev/null 2>&1; then
	echo "Port ${HOSTD_PORT} is already in use, killing existing listener(s)"
	fuser -k -n tcp "${HOSTD_PORT}" >/dev/null 2>&1 || true
	sleep 0.5
fi

setsid "${SCRIPT_DIR}/run_hostd.sh" >"${LOG_FILE}" 2>&1 &
HOSTD_PID=$!

echo "Started hostd (PID: ${HOSTD_PID}), logging to ${LOG_FILE}"

# Wait for hostd to be ready
HOSTD_READY=0
for _ in {1..50}; do
	if ! kill -0 "${HOSTD_PID}" >/dev/null 2>&1; then
		if wait "${HOSTD_PID}"; then
			hostd_exit_status=0
		else
			hostd_exit_status=$?
		fi
		echo "run_hostd.sh exited before hostd became ready (exit status ${hostd_exit_status})"
		echo "--- hostd log ---"
		cat "${LOG_FILE}" || true
		exit "${hostd_exit_status}"
	fi

	if curl -fsS -H "Authorization: Bearer ${HOSTD_TOKEN}" "${HOSTD_URL}/api/health" >/dev/null 2>&1; then
		HOSTD_READY=1
		break
	fi
	sleep 0.2
done

if [[ "${HOSTD_READY}" -ne 1 ]]; then
	echo "hostd did not become ready within timeout"
	echo "--- hostd log ---"
	cat "${LOG_FILE}" || true
	exit 1
fi

# Create a VM using the hostd API
CREATE_RESPONSE="$(curl -fsS \
	-X POST "${HOSTD_URL}/api/vms" \
	-H "Authorization: Bearer ${HOSTD_TOKEN}" \
	-H "Content-Type: application/json" \
	-d '{
		"name": "e2e-vm",
		"project_id": 123,
        "mode": "ephemeral",
		"image": "ubuntu-24",
		"cpus": 1,
		"memory_mb": 512,
		"disk_size_mb": 1024,
		"network_config": {
			"allow_internet": true,
			"ingress_ports": [],
			"egress": [],
			"public_access": false
		},
		"ssh_access": false,
		"env": [],
		"cmd": [],
		"services": [],
		"cron_schedule": null,
        "tags": []
	}')"

echo "Create response: ${CREATE_RESPONSE}"

VM_ID="$(jq -r '.id' <<<"${CREATE_RESPONSE}")"
if [[ -z "${VM_ID}" || "${VM_ID}" == "null" ]]; then
	echo "Failed to extract vm id from create response"
	exit 1
fi
echo "Created VM: ${VM_ID}"

# Get the VM, expecting its id, state, and config to round-trip. The create
# endpoint also boots the VM, so the stable state is "started", not "created".
GET_RESPONSE="$(curl -fsS \
	-H "Authorization: Bearer ${HOSTD_TOKEN}" \
	"${HOSTD_URL}/api/vms/${VM_ID}")"
echo "Get response: ${GET_RESPONSE}"

GET_ID="$(jq -r '.vm_id' <<<"${GET_RESPONSE}")"
GET_STATUS="$(jq -r '.state' <<<"${GET_RESPONSE}")"
GET_NAME="$(jq -r '.vm_config.name' <<<"${GET_RESPONSE}")"
if [[ "${GET_ID}" != "${VM_ID}" || "${GET_STATUS}" != "started" || "${GET_NAME}" != "e2e-vm" ]]; then
	echo "Unexpected get response: ${GET_RESPONSE}"
	exit 1
fi
echo "Got VM ${VM_ID} (status: ${GET_STATUS})"

# hostd now waits for Firecracker to report the VM as "Running" before it
# marks the VM as "started", so once the state above is "started" the
# Firecracker API on the VM's socket must agree.
FC_SOCKET="/tmp/tikovm/${VM_ID}/${VM_ID}.socket"
FC_INFO="$(curl -fsS --unix-socket "${FC_SOCKET}" http://localhost/)"
echo "Firecracker instance info: ${FC_INFO}"
FC_STATE="$(jq -r '.state' <<<"${FC_INFO}")"
if [[ "${FC_STATE}" != "Running" ]]; then
	echo "Expected Firecracker state 'Running' for a 'started' VM, got: ${FC_INFO}"
	exit 1
fi
echo "Firecracker reports VM ${VM_ID} as Running"

# Check the serial console output to verify the VM actually boots: the
# initramfs assembles the overlay root (needs /dev/vda + /dev/vdb) and
# switch_roots into systemd, which eventually starts a getty on ttyS0.
SERIAL_LOG="/tmp/tikovm/${VM_ID}/${VM_ID}.serial.log"
BOOT_OK=0
for _ in {1..300}; do
	if [[ -f "${SERIAL_LOG}" ]]; then
		if grep -q "dropping to rescue shell" "${SERIAL_LOG}"; then
			echo "VM boot failed: init dropped to rescue shell"
			echo "--- serial console log (${SERIAL_LOG}) ---"
			cat "${SERIAL_LOG}"
			exit 1
		fi
		if grep -q "login:" "${SERIAL_LOG}"; then
			BOOT_OK=1
			break
		fi
	fi
	sleep 0.2
done

if [[ "${BOOT_OK}" -ne 1 ]]; then
	echo "VM did not reach a login prompt within 60s"
	echo "--- serial console log (${SERIAL_LOG}) ---"
	cat "${SERIAL_LOG}" 2>/dev/null || true
	exit 1
fi
echo "VM ${VM_ID} booted to a login prompt"

# Pause the VM, expecting the API and Firecracker to agree on the paused state
PAUSE_RESPONSE="$(curl -fsS \
	-X POST "${HOSTD_URL}/api/vms/${VM_ID}/pause" \
	-H "Authorization: Bearer ${HOSTD_TOKEN}")"
echo "Pause response: ${PAUSE_RESPONSE}"

PAUSE_STATUS="$(jq -r '.state' <<<"${PAUSE_RESPONSE}")"
if [[ "${PAUSE_STATUS}" != "paused" ]]; then
	echo "Unexpected pause response: ${PAUSE_RESPONSE}"
	exit 1
fi

FC_INFO="$(curl -fsS --unix-socket "${FC_SOCKET}" http://localhost/)"
FC_STATE="$(jq -r '.state' <<<"${FC_INFO}")"
if [[ "${FC_STATE}" != "Paused" ]]; then
	echo "Expected Firecracker state 'Paused' for a 'paused' VM, got: ${FC_INFO}"
	exit 1
fi
echo "VM ${VM_ID} paused (Firecracker agrees)"

# Resume the VM, expecting it to end up back in the started/running state
RESUME_RESPONSE="$(curl -fsS \
	-X POST "${HOSTD_URL}/api/vms/${VM_ID}/resume" \
	-H "Authorization: Bearer ${HOSTD_TOKEN}")"
echo "Resume response: ${RESUME_RESPONSE}"

RESUME_STATUS="$(jq -r '.state' <<<"${RESUME_RESPONSE}")"
if [[ "${RESUME_STATUS}" != "started" ]]; then
	echo "Unexpected resume response: ${RESUME_RESPONSE}"
	exit 1
fi

FC_INFO="$(curl -fsS --unix-socket "${FC_SOCKET}" http://localhost/)"
FC_STATE="$(jq -r '.state' <<<"${FC_INFO}")"
if [[ "${FC_STATE}" != "Running" ]]; then
	echo "Expected Firecracker state 'Running' for a resumed VM, got: ${FC_INFO}"
	exit 1
fi
echo "VM ${VM_ID} resumed (Firecracker reports Running)"

# Resuming a VM that is not paused must fail with the uniform JSON error
RESUME_AGAIN_RESPONSE="$(curl -sS -w '\n%{http_code}' \
	-X POST "${HOSTD_URL}/api/vms/${VM_ID}/resume" \
	-H "Authorization: Bearer ${HOSTD_TOKEN}")"
RESUME_AGAIN_CODE="${RESUME_AGAIN_RESPONSE##*$'\n'}"
RESUME_AGAIN_BODY="${RESUME_AGAIN_RESPONSE%$'\n'*}"

if [[ "${RESUME_AGAIN_CODE}" != "500" ]]; then
	echo "Expected 500 when resuming a running vm, got ${RESUME_AGAIN_CODE}"
	echo "${RESUME_AGAIN_BODY}"
	exit 1
fi

RESUME_ERROR_CODE="$(jq -r '.error.code' <<<"${RESUME_AGAIN_BODY}")"
if [[ "${RESUME_ERROR_CODE}" != "500" ]]; then
	echo "Unexpected error body: ${RESUME_AGAIN_BODY}"
	exit 1
fi
echo "Second resume returned expected error: ${RESUME_AGAIN_BODY}"

# Snapshot the running VM, expecting it to end up suspended with the
# snapshot files written to the work dir
SNAP_RESPONSE="$(curl -fsS \
	-X POST "${HOSTD_URL}/api/vms/${VM_ID}/snapshot" \
	-H "Authorization: Bearer ${HOSTD_TOKEN}")"
echo "Snapshot response: ${SNAP_RESPONSE}"

SNAP_STATE_PATH="$(jq -r '.state_path' <<<"${SNAP_RESPONSE}")"
SNAP_MEM_PATH="$(jq -r '.mem_path' <<<"${SNAP_RESPONSE}")"
if [[ ! -s "${SNAP_STATE_PATH}" || ! -s "${SNAP_MEM_PATH}" ]]; then
	echo "Snapshot files missing or empty: ${SNAP_STATE_PATH} ${SNAP_MEM_PATH}"
	exit 1
fi

GET_RESPONSE="$(curl -fsS \
	-H "Authorization: Bearer ${HOSTD_TOKEN}" \
	"${HOSTD_URL}/api/vms/${VM_ID}")"
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
SNAP_AGAIN_RESPONSE="$(curl -sS -w '\n%{http_code}' \
	-X POST "${HOSTD_URL}/api/vms/${VM_ID}/snapshot" \
	-H "Authorization: Bearer ${HOSTD_TOKEN}")"
SNAP_AGAIN_CODE="${SNAP_AGAIN_RESPONSE##*$'\n'}"
SNAP_AGAIN_BODY="${SNAP_AGAIN_RESPONSE%$'\n'*}"

if [[ "${SNAP_AGAIN_CODE}" != "500" ]]; then
	echo "Expected 500 when snapshotting a suspended vm, got ${SNAP_AGAIN_CODE}"
	echo "${SNAP_AGAIN_BODY}"
	exit 1
fi

SNAP_ERROR_CODE="$(jq -r '.error.code' <<<"${SNAP_AGAIN_BODY}")"
if [[ "${SNAP_ERROR_CODE}" != "500" ]]; then
	echo "Unexpected error body: ${SNAP_AGAIN_BODY}"
	exit 1
fi
echo "Second snapshot returned expected error: ${SNAP_AGAIN_BODY}"

# Restore the VM from its snapshot, expecting it back in started/running
RESTORE_RESPONSE="$(curl -fsS \
	-X POST "${HOSTD_URL}/api/vms/${VM_ID}/restore" \
	-H "Authorization: Bearer ${HOSTD_TOKEN}")"
echo "Restore response: ${RESTORE_RESPONSE}"

RESTORE_STATUS="$(jq -r '.state' <<<"${RESTORE_RESPONSE}")"
if [[ "${RESTORE_STATUS}" != "started" ]]; then
	echo "Unexpected restore response: ${RESTORE_RESPONSE}"
	exit 1
fi

FC_INFO="$(curl -fsS --unix-socket "${FC_SOCKET}" http://localhost/)"
FC_STATE="$(jq -r '.state' <<<"${FC_INFO}")"
if [[ "${FC_STATE}" != "Running" ]]; then
	echo "Expected Firecracker state 'Running' for a restored VM, got: ${FC_INFO}"
	exit 1
fi
echo "VM ${VM_ID} restored from snapshot (Firecracker reports Running)"

# Restoring a VM that is not suspended must fail with the uniform JSON error
RESTORE_AGAIN_RESPONSE="$(curl -sS -w '\n%{http_code}' \
	-X POST "${HOSTD_URL}/api/vms/${VM_ID}/restore" \
	-H "Authorization: Bearer ${HOSTD_TOKEN}")"
RESTORE_AGAIN_CODE="${RESTORE_AGAIN_RESPONSE##*$'\n'}"
RESTORE_AGAIN_BODY="${RESTORE_AGAIN_RESPONSE%$'\n'*}"

if [[ "${RESTORE_AGAIN_CODE}" != "500" ]]; then
	echo "Expected 500 when restoring a running vm, got ${RESTORE_AGAIN_CODE}"
	echo "${RESTORE_AGAIN_BODY}"
	exit 1
fi

RESTORE_ERROR_CODE="$(jq -r '.error.code' <<<"${RESTORE_AGAIN_BODY}")"
if [[ "${RESTORE_ERROR_CODE}" != "500" ]]; then
	echo "Unexpected error body: ${RESTORE_AGAIN_BODY}"
	exit 1
fi
echo "Second restore returned expected error: ${RESTORE_AGAIN_BODY}"

# List VMs, expecting the created VM to be present
LIST_RESPONSE="$(curl -fsS \
	-H "Authorization: Bearer ${HOSTD_TOKEN}" \
	"${HOSTD_URL}/api/vms")"
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
DELETE_CODE="$(curl -sS -o /dev/null -w '%{http_code}' \
	-X DELETE "${HOSTD_URL}/api/vms/${VM_ID}" \
	-H "Authorization: Bearer ${HOSTD_TOKEN}")"
if [[ "${DELETE_CODE}" != "204" ]]; then
	echo "Expected 204 from delete, got ${DELETE_CODE}"
	exit 1
fi
echo "Deleted VM ${VM_ID} (HTTP ${DELETE_CODE})"

# Getting the deleted VM should fail with a uniform 404 JSON error
GET_DELETED_RESPONSE="$(curl -sS -w '\n%{http_code}' \
	-H "Authorization: Bearer ${HOSTD_TOKEN}" \
	"${HOSTD_URL}/api/vms/${VM_ID}")"
GET_DELETED_CODE="${GET_DELETED_RESPONSE##*$'\n'}"
GET_DELETED_BODY="${GET_DELETED_RESPONSE%$'\n'*}"

if [[ "${GET_DELETED_CODE}" != "404" ]]; then
	echo "Expected 404 when getting a deleted vm, got ${GET_DELETED_CODE}"
	echo "${GET_DELETED_BODY}"
	exit 1
fi

GET_ERROR_CODE="$(jq -r '.error.code' <<<"${GET_DELETED_BODY}")"
if [[ "${GET_ERROR_CODE}" != "404" ]]; then
	echo "Unexpected error body: ${GET_DELETED_BODY}"
	exit 1
fi
echo "Get after delete returned expected 404 JSON error: ${GET_DELETED_BODY}"

# The VM's whole work dir (socket, logs, overlay disk, snapshot files)
# should be cleaned up
if [[ -e "/tmp/tikovm/${VM_ID}" ]]; then
	echo "VM work dir /tmp/tikovm/${VM_ID} was not cleaned up"
	exit 1
fi

# Deleting the same VM again should fail with a uniform 404 JSON error
DELETE_AGAIN_RESPONSE="$(curl -sS -w '\n%{http_code}' \
	-X DELETE "${HOSTD_URL}/api/vms/${VM_ID}" \
	-H "Authorization: Bearer ${HOSTD_TOKEN}")"
DELETE_AGAIN_CODE="${DELETE_AGAIN_RESPONSE##*$'\n'}"
DELETE_AGAIN_BODY="${DELETE_AGAIN_RESPONSE%$'\n'*}"

if [[ "${DELETE_AGAIN_CODE}" != "404" ]]; then
	echo "Expected 404 when deleting a missing vm, got ${DELETE_AGAIN_CODE}"
	echo "${DELETE_AGAIN_BODY}"
	exit 1
fi

ERROR_CODE="$(jq -r '.error.code' <<<"${DELETE_AGAIN_BODY}")"
ERROR_MESSAGE="$(jq -r '.error.message' <<<"${DELETE_AGAIN_BODY}")"
if [[ "${ERROR_CODE}" != "404" || -z "${ERROR_MESSAGE}" || "${ERROR_MESSAGE}" == "null" ]]; then
	echo "Unexpected error body: ${DELETE_AGAIN_BODY}"
	exit 1
fi
echo "Second delete returned expected 404 JSON error: ${DELETE_AGAIN_BODY}"

# The deleted VM should no longer be listed
LIST_AFTER_DELETE="$(curl -fsS \
	-H "Authorization: Bearer ${HOSTD_TOKEN}" \
	"${HOSTD_URL}/api/vms")"
LIST_AFTER_DELETE_COUNT="$(jq 'length' <<<"${LIST_AFTER_DELETE}")"
if [[ "${LIST_AFTER_DELETE_COUNT}" != "0" ]]; then
	echo "Expected empty VM list after delete, got: ${LIST_AFTER_DELETE}"
	exit 1
fi
echo "VM list is empty after delete"

printf '\ne2e test passed. ✅\n\n'
