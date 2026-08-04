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
	fuser -k -n tcp "${HOSTD_PORT}" >/dev/null 2>&1 || sudo fuser -k -n tcp "${HOSTD_PORT}" >/dev/null 2>&1 || true
	sleep 0.5
fi

setsid "${SCRIPT_DIR}/../scripts/run_hostd.sh" >"${LOG_FILE}" 2>&1 &
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
FC_INFO="$(sudo curl -fsS --unix-socket "${FC_SOCKET}" http://localhost/)"
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

# --- Workloads: run commands in the guest via guestd over vsock. ---
# Start a workload, wait for it to exit, and check its result and logs.
WL_CREATE="$(curl -fsS \
	-X POST "${HOSTD_URL}/api/vms/${VM_ID}/workloads" \
	-H "Authorization: Bearer ${HOSTD_TOKEN}" \
	-H "Content-Type: application/json" \
	-d '{"argv":["sh","-c","echo hello; sleep 2; echo done; exit 3"],"env":[],"cwd":null}')"
echo "Workload create response: ${WL_CREATE}"

WL_ID="$(jq -r '.workload_id' <<<"${WL_CREATE}")"
if [[ -z "${WL_ID}" || "${WL_ID}" == "null" ]]; then
	echo "Failed to extract workload id from create response"
	exit 1
fi

WL_DONE=0
for _ in {1..60}; do
	WL_GET="$(curl -fsS \
		-H "Authorization: Bearer ${HOSTD_TOKEN}" \
		"${HOSTD_URL}/api/vms/${VM_ID}/workloads/${WL_ID}")"
	WL_STATE="$(jq -r '.state' <<<"${WL_GET}")"
	if [[ "${WL_STATE}" == "exited" ]]; then
		WL_DONE=1
		break
	fi
	if [[ "${WL_STATE}" == "failed" ]]; then
		echo "Workload ${WL_ID} failed to start: ${WL_GET}"
		exit 1
	fi
	sleep 0.5
done

if [[ "${WL_DONE}" -ne 1 ]]; then
	echo "Workload ${WL_ID} did not exit within timeout (last state: ${WL_STATE})"
	exit 1
fi

WL_EXIT_CODE="$(jq -r '.exit_code' <<<"${WL_GET}")"
if [[ "${WL_EXIT_CODE}" != "3" ]]; then
	echo "Expected exit_code 3 for workload ${WL_ID}, got: ${WL_GET}"
	exit 1
fi
echo "Workload ${WL_ID} exited with expected exit code 3"

WL_LOGS="$(curl -fsS \
	-H "Authorization: Bearer ${HOSTD_TOKEN}" \
	"${HOSTD_URL}/api/vms/${VM_ID}/workloads/${WL_ID}/logs")"
if ! grep -q "hello" <<<"${WL_LOGS}" || ! grep -q "done" <<<"${WL_LOGS}"; then
	echo "Workload logs missing expected output: ${WL_LOGS}"
	exit 1
fi
echo "Workload ${WL_ID} logs contain expected output"

# A long-running workload can be stopped, ending up in state "stopped".
WL2_CREATE="$(curl -fsS \
	-X POST "${HOSTD_URL}/api/vms/${VM_ID}/workloads" \
	-H "Authorization: Bearer ${HOSTD_TOKEN}" \
	-H "Content-Type: application/json" \
	-d '{"argv":["sleep","300"],"env":[],"cwd":null}')"
WL2_ID="$(jq -r '.workload_id' <<<"${WL2_CREATE}")"
if [[ -z "${WL2_ID}" || "${WL2_ID}" == "null" ]]; then
	echo "Failed to extract workload id from create response: ${WL2_CREATE}"
	exit 1
fi

WL2_RUNNING=0
for _ in {1..20}; do
	WL2_STATE="$(curl -fsS \
		-H "Authorization: Bearer ${HOSTD_TOKEN}" \
		"${HOSTD_URL}/api/vms/${VM_ID}/workloads/${WL2_ID}" | jq -r '.state')"
	if [[ "${WL2_STATE}" == "running" ]]; then
		WL2_RUNNING=1
		break
	fi
	sleep 0.5
done
if [[ "${WL2_RUNNING}" -ne 1 ]]; then
	echo "Workload ${WL2_ID} did not reach running state (last state: ${WL2_STATE})"
	exit 1
fi

curl -fsS \
	-X POST "${HOSTD_URL}/api/vms/${VM_ID}/workloads/${WL2_ID}/stop" \
	-H "Authorization: Bearer ${HOSTD_TOKEN}" >/dev/null

WL2_STOPPED=0
for _ in {1..20}; do
	WL2_STATE="$(curl -fsS \
		-H "Authorization: Bearer ${HOSTD_TOKEN}" \
		"${HOSTD_URL}/api/vms/${VM_ID}/workloads/${WL2_ID}" | jq -r '.state')"
	if [[ "${WL2_STATE}" == "stopped" ]]; then
		WL2_STOPPED=1
		break
	fi
	sleep 0.5
done
if [[ "${WL2_STOPPED}" -ne 1 ]]; then
	echo "Workload ${WL2_ID} did not stop (last state: ${WL2_STATE})"
	exit 1
fi
echo "Workload ${WL2_ID} stopped on request"

# Both workloads are listed for the VM.
WL_LIST="$(curl -fsS \
	-H "Authorization: Bearer ${HOSTD_TOKEN}" \
	"${HOSTD_URL}/api/vms/${VM_ID}/workloads")"
WL_LIST_COUNT="$(jq 'length' <<<"${WL_LIST}")"
if [[ "${WL_LIST_COUNT}" != "2" ]]; then
	echo "Expected 2 workloads in list, got: ${WL_LIST}"
	exit 1
fi
echo "Workload list contains both workloads"

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

FC_INFO="$(sudo curl -fsS --unix-socket "${FC_SOCKET}" http://localhost/)"
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

FC_INFO="$(sudo curl -fsS --unix-socket "${FC_SOCKET}" http://localhost/)"
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

FC_INFO="$(sudo curl -fsS --unix-socket "${FC_SOCKET}" http://localhost/)"
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

# --- Networking: a second VM in the same project must land in the same ---
# --- subnet, on the same per-project bridge, with a distinct guest IP.   ---
NET1_RESPONSE="$(curl -fsS \
	-H "Authorization: Bearer ${HOSTD_TOKEN}" \
	"${HOSTD_URL}/api/vms/${VM_ID}")"
VM1_SUBNET="$(jq -r '.net.subnet' <<<"${NET1_RESPONSE}")"
VM1_GUEST_IP="$(jq -r '.net.guest_ip' <<<"${NET1_RESPONSE}")"
VM1_TAP="$(jq -r '.net.tap_name' <<<"${NET1_RESPONSE}")"
if [[ -z "${VM1_SUBNET}" || "${VM1_SUBNET}" == "null" ]]; then
	echo "VM ${VM_ID} has no network allocation: ${NET1_RESPONSE}"
	exit 1
fi

CREATE2_RESPONSE="$(curl -fsS \
	-X POST "${HOSTD_URL}/api/vms" \
	-H "Authorization: Bearer ${HOSTD_TOKEN}" \
	-H "Content-Type: application/json" \
	-d '{
		"name": "e2e-vm-2",
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
VM2_ID="$(jq -r '.id' <<<"${CREATE2_RESPONSE}")"
if [[ -z "${VM2_ID}" || "${VM2_ID}" == "null" ]]; then
	echo "Failed to extract second vm id from create response: ${CREATE2_RESPONSE}"
	exit 1
fi

NET2_RESPONSE="$(curl -fsS \
	-H "Authorization: Bearer ${HOSTD_TOKEN}" \
	"${HOSTD_URL}/api/vms/${VM2_ID}")"
VM2_SUBNET="$(jq -r '.net.subnet' <<<"${NET2_RESPONSE}")"
VM2_GUEST_IP="$(jq -r '.net.guest_ip' <<<"${NET2_RESPONSE}")"
VM2_TAP="$(jq -r '.net.tap_name' <<<"${NET2_RESPONSE}")"
if [[ "${VM2_SUBNET}" != "${VM1_SUBNET}" ]]; then
	echo "VMs of the same project got different subnets: ${VM1_SUBNET} vs ${VM2_SUBNET}"
	exit 1
fi
if [[ "${VM2_GUEST_IP}" == "${VM1_GUEST_IP}" ]]; then
	echo "VMs of the same project got the same guest IP: ${VM1_GUEST_IP}"
	exit 1
fi
echo "Both VMs share subnet ${VM1_SUBNET} (guest IPs ${VM1_GUEST_IP}, ${VM2_GUEST_IP})"

# Host topology: one bridge for the project, one TAP per VM enslaved to it.
BRIDGE="tbr-123"
if ! ip link show "${BRIDGE}" >/dev/null 2>&1; then
	echo "Project bridge ${BRIDGE} does not exist"
	exit 1
fi
for TAP in "${VM1_TAP}" "${VM2_TAP}"; do
	if ! ip link show "${TAP}" >/dev/null 2>&1; then
		echo "TAP device ${TAP} does not exist"
		exit 1
	fi
	TAP_MASTER="$(ip -o link show "${TAP}" | grep -oP 'master \K\S+' || true)"
	if [[ "${TAP_MASTER}" != "${BRIDGE}" ]]; then
		echo "TAP ${TAP} is not enslaved to ${BRIDGE} (master: ${TAP_MASTER:-none})"
		exit 1
	fi
done
echo "Bridge ${BRIDGE} carries TAPs ${VM1_TAP} and ${VM2_TAP}"

# Data path check: both guests answer ping from the host over the bridge.
# VM ${VM_ID} went through snapshot/restore above, so this also proves
# networking survives a restore: the guest resumes from the memory snapshot
# with eth0 already configured and its TAP was never torn down.
for IP in "${VM1_GUEST_IP}" "${VM2_GUEST_IP}"; do
	PING_OK=0
	for _ in {1..15}; do
		if ping -c 1 -W 1 "${IP}" >/dev/null 2>&1; then
			PING_OK=1
			break
		fi
		sleep 1
	done
	if [[ "${PING_OK}" -ne 1 ]]; then
		echo "Guest ${IP} does not answer ping from the host"
		exit 1
	fi
done
echo "Both guests answer ping from the host (${VM_ID} post-restore)"

# Deleting the second VM must release its TAP but keep the bridge alive
# while the first VM still uses it.
DELETE2_CODE="$(curl -sS -o /dev/null -w '%{http_code}' \
	-X DELETE "${HOSTD_URL}/api/vms/${VM2_ID}" \
	-H "Authorization: Bearer ${HOSTD_TOKEN}")"
if [[ "${DELETE2_CODE}" != "204" ]]; then
	echo "Expected 204 from deleting ${VM2_ID}, got ${DELETE2_CODE}"
	exit 1
fi
if ip link show "${VM2_TAP}" >/dev/null 2>&1; then
	echo "TAP ${VM2_TAP} still exists after deleting ${VM2_ID}"
	exit 1
fi
if ! ip link show "${BRIDGE}" >/dev/null 2>&1; then
	echo "Bridge ${BRIDGE} was torn down while VM ${VM_ID} is still running"
	exit 1
fi
echo "Deleted VM ${VM2_ID}; bridge ${BRIDGE} kept alive for VM ${VM_ID}"

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

# With the project's last VM gone, its bridge, TAP and subnet must be torn
# down as well.
if ip link show "${BRIDGE}" >/dev/null 2>&1; then
	echo "Bridge ${BRIDGE} still exists after deleting the project's last VM"
	exit 1
fi
if ip link show "${VM1_TAP}" >/dev/null 2>&1; then
	echo "TAP ${VM1_TAP} still exists after deleting ${VM_ID}"
	exit 1
fi
echo "Bridge ${BRIDGE} and TAP ${VM1_TAP} torn down with the project's last VM"

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
