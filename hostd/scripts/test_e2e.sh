#!/bin/bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

HOSTD_ADDR="127.0.0.1:3000"
HOSTD_URL="http://${HOSTD_ADDR}"
HOSTD_TOKEN="${TIKOVM_HOSTD_API_TOKEN:-xxx}"
LOG_FILE="$(mktemp -t tikovm-hostd-e2e.XXXXXX.log)"

cleanup() {
	if [[ -n "${HOSTD_PID:-}" ]]; then
		kill "${HOSTD_PID}" >/dev/null 2>&1 || true
		wait "${HOSTD_PID}" >/dev/null 2>&1 || true
	fi
}

trap cleanup EXIT

"${SCRIPT_DIR}/run_hostd.sh" >"${LOG_FILE}" 2>&1 &
HOSTD_PID=$!

# Wait for hostd to be ready
for _ in {1..50}; do
	if curl -fsS -H "Authorization: Bearer ${HOSTD_TOKEN}" "${HOSTD_URL}/api/health" >/dev/null 2>&1; then
		break
	fi
	sleep 0.2
done

# Create a VM using the hostd API
curl -fsS \
	-X POST "${HOSTD_URL}/api/vms" \
	-H "Authorization: Bearer ${HOSTD_TOKEN}" \
	-H "Content-Type: application/json" \
	-d '{
		"name": "e2e-vm",
		"image": "alpine",
		"project": "e2e",
		"mode": "ephemeral",
		"config": {
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
			"cron_schedule": null
		}
	}'

