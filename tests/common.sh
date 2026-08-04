# shellcheck shell=bash
#
# Shared helpers for the end-to-end test files in this directory. This file
# is meant to be sourced, not executed: each test file sets its own
# `set -euo pipefail`, sources this file, installs
# `trap 'cleanup_vms; stop_hostd' EXIT` and then calls `start_hostd`.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

export FIRECRACKER_BIN="${HOME}/firecracker/build/cargo_target/x86_64-unknown-linux-musl/debug/firecracker"

HOSTD_ADDR="127.0.0.1:3000"
HOSTD_URL="http://${HOSTD_ADDR}"
HOSTD_PORT="${HOSTD_ADDR##*:}"
# The proxy server (--proxy-listen default) forwards to guest exposed ports.
PROXY_ADDR="127.0.0.1:8080"
PROXY_URL="http://${PROXY_ADDR}"
HOSTD_TOKEN="${TIKOVM_HOSTD_API_TOKEN:-xxx}"
LOG_FILE="$(mktemp -t tikovm-hostd-e2e.XXXXXX.log)"

# Set by start_hostd; VM ids the test created, for best-effort cleanup.
HOSTD_PID=""
CREATED_VMS=()

# --- hostd process management ----------------------------------------------

start_hostd() {
	# Make sure no stale process (e.g. left over from a previous manual run) is
	# already bound to the API or proxy port, otherwise the readiness check
	# below could pass against that stale server instead of the instance this
	# script starts. hostd runs as root via sudo, so a non-root fuser cannot
	# see its sockets — check with sudo.
	local port
	for port in "${HOSTD_PORT}" "${PROXY_ADDR##*:}"; do
		if sudo fuser -n tcp "${port}" >/dev/null 2>&1; then
			echo "Port ${port} is already in use, killing existing listener(s)"
			sudo fuser -k -n tcp "${port}" >/dev/null 2>&1 || true
			sleep 0.5
		fi
	done

	setsid "${REPO_ROOT}/scripts/run_hostd.sh" >"${LOG_FILE}" 2>&1 &
	HOSTD_PID=$!

	echo "Started hostd (PID: ${HOSTD_PID}), logging to ${LOG_FILE}"

	# Wait for hostd to be ready
	local ready=0
	for _ in {1..50}; do
		if ! kill -0 "${HOSTD_PID}" >/dev/null 2>&1; then
			local status=0
			wait "${HOSTD_PID}" || status=$?
			echo "run_hostd.sh exited before hostd became ready (exit status ${status})"
			echo "--- hostd log ---"
			cat "${LOG_FILE}" || true
			exit "${status}"
		fi

		if curl -fsS -H "Authorization: Bearer ${HOSTD_TOKEN}" "${HOSTD_URL}/api/health" >/dev/null 2>&1; then
			ready=1
			break
		fi
		sleep 0.2
	done

	if [[ "${ready}" -ne 1 ]]; then
		echo "hostd did not become ready within timeout"
		echo "--- hostd log ---"
		cat "${LOG_FILE}" || true
		exit 1
	fi
}

stop_hostd() {
	if [[ -n "${HOSTD_PID}" ]]; then
		# HOSTD_PID is a process group leader (started via setsid), so kill the
		# whole group to make sure cargo and the hostd binary it spawns are
		# both terminated instead of being left behind as orphans.
		kill -TERM -- "-${HOSTD_PID}" >/dev/null 2>&1 || true
		wait "${HOSTD_PID}" >/dev/null 2>&1 || true
		HOSTD_PID=""
	fi
}

# --- VM registration / best-effort cleanup ----------------------------------
# A test that fails halfway would otherwise leave its VMs (and the project
# bridge/TAPs behind them) running, breaking any test file that runs next.

register_vm() {
	CREATED_VMS+=("$1")
}

cleanup_vms() {
	local id
	for id in ${CREATED_VMS[@]+"${CREATED_VMS[@]}"}; do
		curl -sS -o /dev/null -X DELETE \
			-H "Authorization: Bearer ${HOSTD_TOKEN}" \
			"${HOSTD_URL}/api/vms/${id}" >/dev/null 2>&1 || true
	done
}

# --- API helpers -------------------------------------------------------------
# All helpers authenticate with the Bearer token; api_* fail on HTTP errors
# (curl -f), api_raw returns "body\nhttp_code" so error responses can be
# asserted on.

api_get() {
	curl -fsS -H "Authorization: Bearer ${HOSTD_TOKEN}" "${HOSTD_URL}$1"
}

api_post() {
	local path="$1" data="${2:-}"
	if [[ -n "${data}" ]]; then
		curl -fsS -X POST \
			-H "Authorization: Bearer ${HOSTD_TOKEN}" \
			-H "Content-Type: application/json" \
			-d "${data}" \
			"${HOSTD_URL}${path}"
	else
		curl -fsS -X POST \
			-H "Authorization: Bearer ${HOSTD_TOKEN}" \
			"${HOSTD_URL}${path}"
	fi
}

# api_raw <method> <path> [json] -> "body\nhttp_code" on stdout
api_raw() {
	local method="$1" path="$2" data="${3:-}"
	if [[ -n "${data}" ]]; then
		curl -sS -w '\n%{http_code}' -X "${method}" \
			-H "Authorization: Bearer ${HOSTD_TOKEN}" \
			-H "Content-Type: application/json" \
			-d "${data}" \
			"${HOSTD_URL}${path}"
	else
		curl -sS -w '\n%{http_code}' -X "${method}" \
			-H "Authorization: Bearer ${HOSTD_TOKEN}" \
			"${HOSTD_URL}${path}"
	fi
}

# api_code <raw> -> the http_code part of an api_raw result
api_code() {
	echo "${1##*$'\n'}"
}

# api_body <raw> -> the body part of an api_raw result
api_body() {
	echo "${1%$'\n'*}"
}

# expect_error_code <raw> <code>: assert both the HTTP status and the
# .error.code of the uniform JSON error body.
expect_error_code() {
	local raw="$1" expected="$2"
	local code body err_code
	code="$(api_code "${raw}")"
	body="$(api_body "${raw}")"
	if [[ "${code}" != "${expected}" ]]; then
		echo "Expected HTTP ${expected}, got ${code}"
		echo "${body}"
		exit 1
	fi
	err_code="$(jq -r '.error.code' <<<"${body}")"
	if [[ "${err_code}" != "${expected}" ]]; then
		echo "Unexpected error body: ${body}"
		exit 1
	fi
}

# --- VM helpers ----------------------------------------------------------------

# create_vm <name> [project_id]: POST the standard test VM, register it for
# cleanup and echo its id. The full response goes to stderr for debugging.
create_vm() {
	local name="$1" project_id="${2:-123}"
	local payload response id
	payload="$(cat <<EOF
{
	"name": "${name}",
	"project_id": ${project_id},
	"mode": "ephemeral",
	"image": "ubuntu-24",
	"cpus": 1,
	"memory_mb": 512,
	"disk_size_mb": 1024,
	"network_config": {
		"allow_internet": true,
		"exposed_ports": [],
		"egress": [],
		"public_access": false
	},
	"ssh_access": false,
	"env": [],
	"cmd": [],
	"services": [],
	"cron_schedule": null,
	"tags": []
}
EOF
)"
	response="$(api_post /api/vms "${payload}")"
	echo "Create response: ${response}" >&2
	id="$(jq -r '.id' <<<"${response}")"
	if [[ -z "${id}" || "${id}" == "null" ]]; then
		echo "Failed to extract vm id from create response: ${response}" >&2
		exit 1
	fi
	register_vm "${id}"
	echo "${id}"
}

# fc_info <vm_id>: query the Firecracker API on the VM's socket.
fc_info() {
	local vm_id="$1"
	sudo curl -fsS --unix-socket "/tmp/tikovm/${vm_id}/${vm_id}.socket" http://localhost/
}

# expect_fc_state <vm_id> <state>: hostd keeps its own VM state in sync with
# what Firecracker reports, so both sides are asserted.
expect_fc_state() {
	local vm_id="$1" expected="$2"
	local info state
	info="$(fc_info "${vm_id}")"
	state="$(jq -r '.state' <<<"${info}")"
	if [[ "${state}" != "${expected}" ]]; then
		echo "Expected Firecracker state '${expected}' for VM ${vm_id}, got: ${info}"
		exit 1
	fi
}

# wait_serial_boot <vm_id>: verify the VM actually boots by watching the
# serial console: the initramfs assembles the overlay root (needs /dev/vda +
# /dev/vdb) and switch_roots into systemd, which eventually starts a getty on
# ttyS0. A rescue shell means the overlay setup failed.
wait_serial_boot() {
	local vm_id="$1"
	local serial_log="/tmp/tikovm/${vm_id}/${vm_id}.serial.log"
	for _ in {1..300}; do
		if [[ -f "${serial_log}" ]]; then
			if grep -q "dropping to rescue shell" "${serial_log}"; then
				echo "VM boot failed: init dropped to rescue shell"
				echo "--- serial console log (${serial_log}) ---"
				cat "${serial_log}"
				exit 1
			fi
			if grep -q "login:" "${serial_log}"; then
				return 0
			fi
		fi
		sleep 0.2
	done
	echo "VM ${vm_id} did not reach a login prompt within 60s"
	echo "--- serial console log (${serial_log}) ---"
	cat "${serial_log}" 2>/dev/null || true
	exit 1
}

# wait_workload_state <vm_id> <workload_id> <state> [tries]: poll the workload
# until it reaches the expected state; the last response is left in WL_GET.
wait_workload_state() {
	local vm_id="$1" wl_id="$2" want="$3" tries="${4:-60}"
	local state=""
	for ((i = 0; i < tries; i++)); do
		WL_GET="$(api_get "/api/vms/${vm_id}/workloads/${wl_id}")"
		state="$(jq -r '.state' <<<"${WL_GET}")"
		if [[ "${state}" == "${want}" ]]; then
			return 0
		fi
		if [[ "${state}" == "failed" ]]; then
			echo "Workload ${wl_id} failed to start: ${WL_GET}"
			exit 1
		fi
		sleep 0.5
	done
	echo "Workload ${wl_id} did not reach state '${want}' (last state: ${state})"
	exit 1
}
