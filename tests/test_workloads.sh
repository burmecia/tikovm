#!/bin/bash
#
# Workloads: run commands in the guest via guestd over vsock. Covers exit
# codes, log capture, stopping a long-running workload, and listing.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=tests/common.sh
source "${SCRIPT_DIR}/common.sh"

trap 'cleanup_vms; stop_hostd' EXIT

start_hostd

VM_ID="$(create_vm "e2e-vm")"
echo "Created VM: ${VM_ID}"

# guestd is a systemd service in the guest, so workloads are only possible
# once the VM has booted.
wait_serial_boot "${VM_ID}"
echo "VM ${VM_ID} booted to a login prompt"

# Start a workload, wait for it to exit, and check its result and logs.
WL_CREATE="$(api_post "/api/vms/${VM_ID}/workloads" \
	'{"cmd":["sh","-c","echo hello; sleep 2; echo done; exit 3"],"env":[],"cwd":null}')"
echo "Workload create response: ${WL_CREATE}"

WL_ID="$(jq -r '.workload_id' <<<"${WL_CREATE}")"
if [[ -z "${WL_ID}" || "${WL_ID}" == "null" ]]; then
	echo "Failed to extract workload id from create response"
	exit 1
fi

wait_workload_state "${VM_ID}" "${WL_ID}" "exited" 60

WL_EXIT_CODE="$(jq -r '.exit_code' <<<"${WL_GET}")"
if [[ "${WL_EXIT_CODE}" != "3" ]]; then
	echo "Expected exit_code 3 for workload ${WL_ID}, got: ${WL_GET}"
	exit 1
fi
echo "Workload ${WL_ID} exited with expected exit code 3"

WL_LOGS="$(api_get "/api/vms/${VM_ID}/workloads/${WL_ID}/logs")"
if ! grep -q "hello" <<<"${WL_LOGS}" || ! grep -q "done" <<<"${WL_LOGS}"; then
	echo "Workload logs missing expected output: ${WL_LOGS}"
	exit 1
fi
echo "Workload ${WL_ID} logs contain expected output"

# A long-running workload can be stopped, ending up in state "stopped".
WL2_CREATE="$(api_post "/api/vms/${VM_ID}/workloads" \
	'{"cmd":["sleep","300"],"env":[],"cwd":null}')"
WL2_ID="$(jq -r '.workload_id' <<<"${WL2_CREATE}")"
if [[ -z "${WL2_ID}" || "${WL2_ID}" == "null" ]]; then
	echo "Failed to extract workload id from create response: ${WL2_CREATE}"
	exit 1
fi

wait_workload_state "${VM_ID}" "${WL2_ID}" "running" 20

api_post "/api/vms/${VM_ID}/workloads/${WL2_ID}/stop" >/dev/null

wait_workload_state "${VM_ID}" "${WL2_ID}" "stopped" 20
echo "Workload ${WL2_ID} stopped on request"

# Both workloads are listed for the VM.
WL_LIST="$(api_get "/api/vms/${VM_ID}/workloads")"
WL_LIST_COUNT="$(jq 'length' <<<"${WL_LIST}")"
if [[ "${WL_LIST_COUNT}" != "2" ]]; then
	echo "Expected 2 workloads in list, got: ${WL_LIST}"
	exit 1
fi
echo "Workload list contains both workloads"

printf '\nWorkloads test passed. ✅\n\n'
