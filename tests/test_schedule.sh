#!/bin/bash
#
# Schedule mode: a `schedule` VM defines `cmd` + `cron_schedule` at create
# time and is NOT started on creation. hostd's cron scheduler wakes it
# (start, or restore from snapshot) on every cron fire, runs `cmd` as a
# workload (tagged origin=schedule), then snapshots it back to `suspended`
# so it consumes no resources between runs.
#
# Covers:
#   - create-time validation (missing/invalid cron_schedule, empty cmd,
#     schedule-only fields on a non-schedule VM),
#   - a 15s-cron VM completing multiple runs, each queryable through the
#     workloads API (state, origin, exit_code) with captured logs,
#   - the VM returning to `suspended` between runs,
#   - guest filesystem state persisting across the suspend/restore cycles.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=tests/common.sh
source "${SCRIPT_DIR}/common.sh"

trap 'cleanup_vms; stop_hostd' EXIT

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

BASE_FIELDS='"project_id": 789, "image": "ubuntu-24", "cpus": 1, "memory_mb": 512,
	"disk_size_mb": 1024, "ssh_access": false, "env": [], "services": [], "tags": [],
	"network_config": {"allow_internet": true, "exposed_ports": [], "egress": [], "public_access": false}'

# --- Negative: create-time validation -----------------------------------------

raw="$(api_raw POST /api/vms "{
	\"name\": \"sched-no-cron\", \"mode\": \"schedule\", ${BASE_FIELDS},
	\"cmd\": [\"sh\", \"-c\", \"true\"]
}")"
expect_error_code "${raw}" 500
echo "Schedule VM without cron_schedule rejected: $(api_body "${raw}")"

raw="$(api_raw POST /api/vms "{
	\"name\": \"sched-bad-cron\", \"mode\": \"schedule\", ${BASE_FIELDS},
	\"cmd\": [\"sh\", \"-c\", \"true\"], \"cron_schedule\": \"not a cron\"
}")"
expect_error_code "${raw}" 500
echo "Schedule VM with invalid cron_schedule rejected: $(api_body "${raw}")"

raw="$(api_raw POST /api/vms "{
	\"name\": \"sched-no-cmd\", \"mode\": \"schedule\", ${BASE_FIELDS},
	\"cmd\": [], \"cron_schedule\": \"*/15 * * * * *\"
}")"
expect_error_code "${raw}" 500
echo "Schedule VM with empty cmd rejected: $(api_body "${raw}")"

raw="$(api_raw POST /api/vms "{
	\"name\": \"eph-with-cron\", \"mode\": \"ephemeral\", ${BASE_FIELDS},
	\"cmd\": [], \"cron_schedule\": \"*/15 * * * * *\"
}")"
expect_error_code "${raw}" 500
echo "Ephemeral VM with cron_schedule rejected: $(api_body "${raw}")"

# --- Happy path: cron fires wake the VM, run the cmd, suspend it again --------

# Every 15s (6-field cron with seconds). The cmd appends to a file on the
# VM's persistent overlay disk, so the number of lines doubles as a run
# counter that must survive the suspend/restore cycles.
raw="$(api_raw POST /api/vms "{
	\"name\": \"sched-basic\", \"mode\": \"schedule\", ${BASE_FIELDS},
	\"cmd\": [\"sh\", \"-c\", \"date >> /root/runs.txt; echo scheduled-run\"],
	\"cron_schedule\": \"*/15 * * * * *\"
}")"
if [[ "$(api_code "${raw}")" != "201" ]]; then
	echo "Failed to create schedule VM: $(api_body "${raw}")"
	exit 1
fi
VM_ID="$(api_body "${raw}" | jq -r '.id')"
register_vm "${VM_ID}"
echo "Created schedule VM: ${VM_ID}"

# Schedule VMs are not started at create time; the scheduler wakes them.
state="$(api_get "/api/vms/${VM_ID}" | jq -r '.state')"
if [[ "${state}" != "created" ]]; then
	echo "Schedule VM should not be started at create time, got: ${state}"
	exit 1
fi
echo "Schedule VM stayed in 'created' state after creation"

# Wait for at least two completed scheduled runs. The first run includes a
# full guest boot; later runs restore from the snapshot, which is much
# faster. Fires landing mid-run are skipped (no overlap).
completed=0
for _ in {1..100}; do
	WLS="$(api_get "/api/vms/${VM_ID}/workloads")"
	completed="$(jq '[.[] | select(.origin == "schedule" and .state == "exited")] | length' <<<"${WLS}")"
	if [[ "${completed}" -ge 2 ]]; then
		break
	fi
	sleep 3
done
if [[ "${completed}" -lt 2 ]]; then
	echo "Expected at least 2 completed scheduled runs, got ${completed}"
	echo "${WLS}"
	echo "--- hostd log tail ---"
	tail -n 30 "${LOG_FILE}" || true
	exit 1
fi
echo "Scheduler completed ${completed} runs"

# Every completed run exited 0, is tagged as schedule-triggered, and its
# captured logs are queryable.
bad_exit="$(jq '[.[] | select(.origin == "schedule" and .state == "exited" and .exit_code != 0)] | length' <<<"${WLS}")"
if [[ "${bad_exit}" -ne 0 ]]; then
	echo "Some scheduled runs had a non-zero exit code: ${WLS}"
	exit 1
fi
WL_ID="$(jq -r '[.[] | select(.origin == "schedule")][0].workload_id' <<<"${WLS}")"
LOGS="$(api_get "/api/vms/${VM_ID}/workloads/${WL_ID}/logs")"
if ! jq -r '.[].data' <<<"${LOGS}" | grep -q "scheduled-run"; then
	echo "Scheduled run logs missing the marker: ${LOGS}"
	exit 1
fi
echo "Scheduled runs have origin=schedule, exit_code=0, and captured logs"

# Between runs the VM is suspended (snapshot on disk, no Firecracker process).
wait_vm_state "${VM_ID}" "suspended" 20
echo "VM ${VM_ID} is suspended between runs"

# The cmd's side effect persisted across the suspend/restore cycles: exec
# (which wakes the VM) shows one /root/runs.txt line per completed run.
EXEC_RESP="$(api_post "/api/vms/${VM_ID}/exec" \
	'{"cmd":["sh","-c","wc -l < /root/runs.txt"],"env":[],"cwd":null}')"
lines="$(jq -r '.logs[].data' <<<"${EXEC_RESP}" | tr -d '[:space:]')"
if [[ "${lines}" -lt 2 ]]; then
	echo "Expected /root/runs.txt to have >= 2 lines after ${completed} runs, got: ${lines}"
	echo "${EXEC_RESP}"
	exit 1
fi
echo "Guest state persisted across suspend/restore (${lines} run markers)"

printf '\nSchedule test passed. ✅\n\n'
