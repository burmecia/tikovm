#!/bin/bash
#
# Exec endpoint: POST /api/vms/{id}/exec runs a command inside the guest and
# blocks until it exits, returning the finished workload plus its captured
# logs in one response.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=tests/common.sh
source "${SCRIPT_DIR}/common.sh"

trap 'cleanup_vms; stop_hostd' EXIT

start_hostd

VM_ID="$(create_vm "e2e-vm")"
echo "Created VM: ${VM_ID}"

# exec goes through guestd over vsock, so the VM must have booted.
wait_serial_boot "${VM_ID}"
echo "VM ${VM_ID} booted to a login prompt"

# A command with output on both streams and a non-zero exit code must
# round-trip: terminal state, exit code, guest pid, timestamps, cmd, and
# per-stream log entries.
EXEC_RESPONSE="$(api_post "/api/vms/${VM_ID}/exec" \
	'{"cmd":["sh","-c","echo hello; echo oops >&2; exit 3"],"env":[],"cwd":null}')"
echo "Exec response: ${EXEC_RESPONSE}"

EXEC_STATE="$(jq -r '.state' <<<"${EXEC_RESPONSE}")"
EXEC_EXIT_CODE="$(jq -r '.exit_code' <<<"${EXEC_RESPONSE}")"
EXEC_PID="$(jq -r '.pid' <<<"${EXEC_RESPONSE}")"
if [[ "${EXEC_STATE}" != "exited" || "${EXEC_EXIT_CODE}" != "3" ]]; then
	echo "Expected state=exited exit_code=3, got: ${EXEC_RESPONSE}"
	exit 1
fi
if [[ -z "${EXEC_PID}" || "${EXEC_PID}" == "null" ]]; then
	echo "Expected a guest pid in the exec response: ${EXEC_RESPONSE}"
	exit 1
fi
for TS in created_at started_at finished_at; do
	if [[ "$(jq -r ".${TS}" <<<"${EXEC_RESPONSE}")" == "null" ]]; then
		echo "Expected ${TS} in the exec response: ${EXEC_RESPONSE}"
		exit 1
	fi
done
if [[ "$(jq -r '.spec.cmd[0]' <<<"${EXEC_RESPONSE}")" != "sh" ]]; then
	echo "Expected spec.cmd to round-trip: ${EXEC_RESPONSE}"
	exit 1
fi
if ! jq -e '.logs[] | select(.stream == "stdout") | .data' <<<"${EXEC_RESPONSE}" | grep -q "hello"; then
	echo "Exec logs missing stdout 'hello': ${EXEC_RESPONSE}"
	exit 1
fi
if ! jq -e '.logs[] | select(.stream == "stderr") | .data' <<<"${EXEC_RESPONSE}" | grep -q "oops"; then
	echo "Exec logs missing stderr 'oops': ${EXEC_RESPONSE}"
	exit 1
fi
echo "Exec returned exit code 3 with guest pid ${EXEC_PID} and both output streams"

# A quick successful command exits 0 with its output in the logs.
EXEC2_RESPONSE="$(api_post "/api/vms/${VM_ID}/exec" \
	'{"cmd":["echo","hi"],"env":[],"cwd":null}')"
EXEC2_EXIT_CODE="$(jq -r '.exit_code' <<<"${EXEC2_RESPONSE}")"
if [[ "${EXEC2_EXIT_CODE}" != "0" ]]; then
	echo "Expected exit_code 0, got: ${EXEC2_RESPONSE}"
	exit 1
fi
if ! jq -e '.logs[] | select(.stream == "stdout") | .data' <<<"${EXEC2_RESPONSE}" | grep -q "hi"; then
	echo "Exec logs missing stdout 'hi': ${EXEC2_RESPONSE}"
	exit 1
fi
echo "Quick exec returned exit code 0 with expected output"

# An empty cmd must fail with the uniform JSON error
EXEC_EMPTY_RAW="$(curl -sS -w '\n%{http_code}' -X POST \
	-H "Authorization: Bearer ${HOSTD_TOKEN}" \
	-H "Content-Type: application/json" \
	-d '{"cmd":[],"env":[],"cwd":null}' \
	"${HOSTD_URL}/api/vms/${VM_ID}/exec")"
expect_error_code "${EXEC_EMPTY_RAW}" "500"
echo "Empty cmd returned expected error: $(api_body "${EXEC_EMPTY_RAW}")"

printf '\nExec test passed. ✅\n\n'
