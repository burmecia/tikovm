#!/bin/bash
#
# Auto-suspend for Postgres VMs: a permanent postgres-16 VM with an
# auto_suspend config suspends when the database has no client connections
# or activity, and wakes transparently when a psql connection arrives at
# the proxy. Uses the hostd-defaulted idle_check_cmd (the image's
# /usr/local/bin/tikovm-pg-idle-check) by omitting it from the config.
#
#   1. psql through the proxy works (VM up)
#   2. an open/active connection keeps the VM started past the idle timeout
#   3. once idle, the VM suspends (Firecracker process stops)
#   4. a new proxied psql connection wakes the VM and is served

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=tests/common.sh
source "${SCRIPT_DIR}/common.sh"

trap 'cleanup_vms; stop_hostd' EXIT

if ! command -v psql >/dev/null 2>&1; then
	echo "psql not found on the host; installing postgresql-client"
	sudo apt update -qq
	sudo apt install -y postgresql-client >/dev/null
fi

start_hostd

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

RESPONSE="$(api_post /api/vms '{
	"name": "pg-auto-suspend", "project_id": 789, "mode": "permanent",
	"image": "postgres-16", "cpus": 1, "memory_mb": 512, "disk_size_mb": 1024,
	"network_config": {"allow_internet": true, "exposed_ports": [{"port": 5432, "label": "postgres"}], "egress": [], "public_access": false},
	"ssh_access": false, "env": [], "cmd": [], "services": [], "cron_schedule": null, "tags": [],
	"auto_suspend": {"idle_timeout_secs": 20, "check_interval_secs": 5}
}')"
echo "Create response: ${RESPONSE}" >&2
VM_ID="$(jq -r '.id' <<<"${RESPONSE}")"
if [[ -z "${VM_ID}" || "${VM_ID}" == "null" ]]; then
	echo "Failed to create VM"
	exit 1
fi
register_vm "${VM_ID}"
echo "Created permanent postgres VM: ${VM_ID}"

wait_serial_boot "${VM_ID}"
echo "VM ${VM_ID} booted"

TOKEN="$(api_post "/api/vms/${VM_ID}/ports/5432/token" '{"ttl_secs": 900, "proto": "tcp"}' | jq -r '.token')"
if [[ -z "${TOKEN}" || "${TOKEN}" == "null" ]]; then
	echo "Failed to mint tcp token"
	exit 1
fi

export PGPASSWORD=postgres
psql_proxy() { # psql_proxy [psql args...]
	PGOPTIONS="-c tikovm_token=${TOKEN}" \
		psql "host=127.0.0.1 port=${PROXY_ADDR##*:} user=postgres dbname=postgres connect_timeout=10" \
		"$@"
}

# 1. Poll until a proxied query succeeds (postgres may still be starting
#    when the login prompt appears).
ok=0
for _ in {1..60}; do
	if psql_proxy -tAc 'select 1' 2>/dev/null | grep -q '^1$'; then
		ok=1
		break
	fi
	sleep 1
done
if [[ "${ok}" -ne 1 ]]; then
	echo "proxied psql 'select 1' did not succeed before suspend"
	exit 1
fi
echo "psql through the proxy works before suspend"

# 2. An open connection with a running query must keep the VM started well
#    past the 20s idle timeout (both the proxy in-flight gate and the guest
#    SQL check see the session).
psql_proxy -c 'select pg_sleep(45)' >/dev/null 2>&1 &
SLEEPER_PID=$!
sleep 30
STATE="$(api_get "/api/vms/${VM_ID}" | jq -r '.state')"
if [[ "${STATE}" != "started" ]]; then
	echo "VM suspended while a connection was active (state: ${STATE})"
	exit 1
fi
echo "VM stayed started with an active connection (past the idle timeout)"
wait "${SLEEPER_PID}" || true

# 3. Idle now: the VM should suspend within the idle timeout + poll margins.
wait_vm_state "${VM_ID}" "suspended" 40
expect_no_fc_process "${VM_ID}"
echo "VM auto-suspended once the database went idle (Firecracker stopped)"

# 4. A new proxied connection wakes the VM; the client just sees a slow
#    connect while the snapshot is restored.
ok=0
for _ in {1..60}; do
	if psql_proxy -tAc 'select 1' 2>/dev/null | grep -q '^1$'; then
		ok=1
		break
	fi
	sleep 2
done
if [[ "${ok}" -ne 1 ]]; then
	echo "proxied psql 'select 1' did not succeed after wake"
	exit 1
fi
wait_vm_state "${VM_ID}" "started" 5
echo "Proxied psql connection woke the suspended VM and was served"

printf '\nPostgres auto-suspend test passed. ✅\n\n'
