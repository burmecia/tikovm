#!/bin/bash
#
# Auto-suspend: idle permanent VMs are snapshotted (Firecracker process
# stopped, zero resources) and transparently woken by the next proxied
# request or exec. Covers both detector paths:
#   - HTTP: the proxy idle timer suspends a VM with exposed ports after
#     idle_timeout_secs without a proxied request; the next proxy request
#     wakes it.
#   - guest: guestd runs the VM's idle_check_cmd and forwards idle events;
#     hostd suspends; exec wakes.
# Also asserts auto_suspend is rejected for non-permanent VMs and that a
# permanent VM without the config never suspends.
#
# NOTE: the guest-detector case needs a guestd with auto-suspend support in
# the guest image; rebuild the rootfs (scripts/rootfs/build_rootfs_*.sh)
# after changing guestd.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=tests/common.sh
source "${SCRIPT_DIR}/common.sh"

trap 'cleanup_vms; stop_hostd' EXIT

start_hostd

# create_permanent_vm <name> <auto_suspend_json|null> [image] [exposed_ports_json]
# Like create_vm but permanent mode and an auto_suspend config (`null` for
# none — the field is an Option, so explicit null is accepted).
create_permanent_vm() {
	local name="$1" auto_suspend="$2" image="${3:-ubuntu-24}" exposed="${4:-[]}"
	local payload response id
	payload="$(cat <<EOF
{
	"name": "${name}",
	"project_id": 456,
	"mode": "permanent",
	"image": "${image}",
	"cpus": 1,
	"memory_mb": 512,
	"disk_size_mb": 1024,
	"network_config": {
		"allow_internet": true,
		"exposed_ports": ${exposed},
		"egress": [],
		"public_access": false
	},
	"ssh_access": false,
	"env": [],
	"cmd": [],
	"services": [],
	"cron_schedule": null,
	"tags": [],
	"auto_suspend": ${auto_suspend}
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

# wait_vm_state <vm_id> <state> <tries>: poll the VM state (3s granularity).
wait_vm_state() {
	local vm_id="$1" want="$2" tries="$3"
	local state="" response
	for ((i = 0; i < tries; i++)); do
		response="$(api_get "/api/vms/${vm_id}")"
		state="$(jq -r '.state' <<<"${response}")"
		if [[ "${state}" == "${want}" ]]; then
			return 0
		fi
		sleep 3
	done
	echo "VM ${vm_id} did not reach state '${want}' (last state: ${state})"
	echo "--- hostd log tail ---"
	tail -n 30 "${LOG_FILE}" || true
	exit 1
}

expect_no_fc_process() {
	local vm_id="$1"
	if pgrep -f "firecracker .*--id ${vm_id}" >/dev/null 2>&1; then
		echo "Firecracker process for ${vm_id} is still running (VM should be suspended)"
		exit 1
	fi
}

# --- Negative: auto_suspend on a non-permanent VM is rejected ----------------

raw="$(api_raw POST /api/vms '{
	"name": "as-ephemeral", "project_id": 456, "mode": "ephemeral",
	"image": "ubuntu-24", "cpus": 1, "memory_mb": 512, "disk_size_mb": 1024,
	"network_config": {"allow_internet": true, "exposed_ports": [], "egress": [], "public_access": false},
	"ssh_access": false, "env": [], "cmd": [], "services": [], "cron_schedule": null, "tags": [],
	"auto_suspend": {"idle_timeout_secs": 30}
}')"
expect_error_code "${raw}" 500
echo "Ephemeral VM with auto_suspend rejected: $(api_body "${raw}")"

# --- Control: a permanent VM without auto_suspend must never suspend --------
# Created first so it has been up well past the idle windows used below when
# asserted at the end of the test.

VM_PLAIN="$(create_permanent_vm "as-plain" "null")"
echo "Created control VM (no auto_suspend): ${VM_PLAIN}"

# --- HTTP detector path: proxy idle timer, proxy wake -------------------------

VM_HTTP="$(create_permanent_vm "as-http" '{"idle_timeout_secs": 30}' "python-3.12" '[{"port": 8080, "label": "web"}]')"
echo "Created HTTP-path VM: ${VM_HTTP}"

wait_serial_boot "${VM_HTTP}"
echo "VM ${VM_HTTP} booted"

api_post "/api/vms/${VM_HTTP}/exec" \
	'{"cmd":["sh","-c","echo hello-auto-suspend > /tmp/hello.txt"],"env":[],"cwd":null}' >/dev/null
WL_CREATE="$(api_post "/api/vms/${VM_HTTP}/workloads" \
	'{"cmd":["python3","-m","http.server","8080","--directory","/tmp"],"env":[],"cwd":null}')"
WL_ID="$(jq -r '.workload_id' <<<"${WL_CREATE}")"
wait_workload_state "${VM_HTTP}" "${WL_ID}" "running" 20

TOKEN="$(api_post "/api/vms/${VM_HTTP}/ports/8080/token" '{"ttl_secs": 900}' | jq -r '.token')"
body=""
for _ in {1..30}; do
	if body="$(curl -fsS -H "Authorization: Bearer ${TOKEN}" "${PROXY_URL}/hello.txt" 2>/dev/null)"; then
		break
	fi
	sleep 0.5
done
if [[ "${body}" != "hello-auto-suspend" ]]; then
	echo "Unexpected proxied body before suspend: ${body}"
	exit 1
fi
echo "Proxied request served before suspend"

# No further requests: the proxy idle timer (30s) plus the 10s poll interval
# should suspend the VM within ~a minute.
wait_vm_state "${VM_HTTP}" "suspended" 30
expect_no_fc_process "${VM_HTTP}"
echo "VM ${VM_HTTP} auto-suspended after the idle timeout (Firecracker stopped)"

# The next proxied request wakes the VM: it just sees a slow first response.
body="$(curl -fsS --max-time 120 -H "Authorization: Bearer ${TOKEN}" "${PROXY_URL}/hello.txt")"
if [[ "${body}" != "hello-auto-suspend" ]]; then
	echo "Unexpected proxied body after wake: ${body}"
	exit 1
fi
wait_vm_state "${VM_HTTP}" "started" 5
echo "Proxied request woke the VM and was served after restore"

# --- Guest detector path: idle_check_cmd, exec wake ---------------------------
# `sh -c "exit 0"` always reports idle, so this VM suspends shortly after
# boot without any exposed port or traffic.

VM_GUEST="$(create_permanent_vm "as-guest" \
	'{"idle_timeout_secs": 10, "idle_check_cmd": ["sh", "-c", "exit 0"], "check_interval_secs": 5}')"
echo "Created guest-detector VM: ${VM_GUEST}"

wait_serial_boot "${VM_GUEST}"
echo "VM ${VM_GUEST} booted; waiting for the guest idle detector to trigger a suspend"

wait_vm_state "${VM_GUEST}" "suspended" 40
expect_no_fc_process "${VM_GUEST}"
echo "VM ${VM_GUEST} auto-suspended on the guest detector signal"

# exec on a suspended VM wakes it first.
EXEC_RESP="$(api_post "/api/vms/${VM_GUEST}/exec" \
	'{"cmd":["sh","-c","echo woke-from-exec"],"env":[],"cwd":null}')"
if ! jq -r '.logs[].data' <<<"${EXEC_RESP}" | grep -q "woke-from-exec"; then
	echo "Unexpected exec response after wake: ${EXEC_RESP}"
	exit 1
fi
wait_vm_state "${VM_GUEST}" "started" 5
echo "exec woke the suspended VM and returned its output"

# --- Negative: a permanent VM without auto_suspend stays up -------------------
# Created at the start of the test; it has been up well past the 30s idle
# window used above.

VM_PLAIN_STATE="$(api_get "/api/vms/${VM_PLAIN}" | jq -r '.state')"
if [[ "${VM_PLAIN_STATE}" != "started" ]]; then
	echo "Permanent VM without auto_suspend should still be started, got: ${VM_PLAIN_STATE}"
	exit 1
fi
echo "Permanent VM without auto_suspend config stayed started"

printf '\nAuto-suspend test passed. ✅\n\n'
