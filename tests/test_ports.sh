#!/bin/bash
#
# End-to-end test for the exposed-ports API: the per-VM registry of labeled
# guest ports for HTTP workloads (list/add/remove, plus an initial set at VM
# creation time). This is VM-side metadata only, so the test never needs the
# guest to boot — every assertion is against the hostd API.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "${SCRIPT_DIR}/common.sh"

trap 'cleanup_vms; stop_hostd' EXIT

start_hostd

# --- Create a VM with two initial exposed ports -------------------------------

payload="$(cat <<'EOF'
{
	"name": "ports-test",
	"project_id": 123,
	"mode": "ephemeral",
	"image": "ubuntu-24",
	"cpus": 1,
	"memory_mb": 512,
	"disk_size_mb": 1024,
	"network_config": {
		"allow_internet": true,
		"exposed_ports": [
			{"port": 8080, "label": "web"},
			{"port": 3000, "label": "grafana"}
		],
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
vm_id="$(jq -r '.id' <<<"${response}")"
if [[ -z "${vm_id}" || "${vm_id}" == "null" ]]; then
	echo "Failed to extract vm id from create response: ${response}"
	exit 1
fi
register_vm "${vm_id}"
echo "Created VM ${vm_id} with initial exposed ports"

# GET /ports lists both initial ports.
ports="$(api_get "/api/vms/${vm_id}/ports")"
if [[ "$(jq -r '[.[].port] | sort | join(",")' <<<"${ports}")" != "3000,8080" ]]; then
	echo "Expected initial ports 3000,8080, got: ${ports}"
	exit 1
fi
if [[ "$(jq -r '.[] | select(.port == 8080) | .label' <<<"${ports}")" != "web" ]]; then
	echo "Expected label 'web' for port 8080, got: ${ports}"
	exit 1
fi

# The same set is visible on the VM resource itself.
vm="$(api_get "/api/vms/${vm_id}")"
if [[ "$(jq -r '.vm_config.network_config.exposed_ports | length' <<<"${vm}")" != "2" ]]; then
	echo "Expected 2 exposed ports on the VM resource, got: ${vm}"
	exit 1
fi

# --- Add ----------------------------------------------------------------------

added="$(api_post "/api/vms/${vm_id}/ports" '{"port": 9090, "label": "metrics"}')"
if [[ "$(jq -r '.port' <<<"${added}")" != "9090" || "$(jq -r '.label' <<<"${added}")" != "metrics" ]]; then
	echo "Unexpected add response: ${added}"
	exit 1
fi
ports="$(api_get "/api/vms/${vm_id}/ports")"
if [[ "$(jq -r 'length' <<<"${ports}")" != "3" ]]; then
	echo "Expected 3 ports after add, got: ${ports}"
	exit 1
fi

# Adding a duplicate port is a 409 conflict.
raw="$(api_raw POST "/api/vms/${vm_id}/ports" '{"port": 8080, "label": "dup"}')"
expect_error_code "${raw}" 409

# Port 0 is invalid.
raw="$(api_raw POST "/api/vms/${vm_id}/ports" '{"port": 0, "label": "bad"}')"
expect_error_code "${raw}" 400

# --- Remove -------------------------------------------------------------------

raw="$(api_raw DELETE "/api/vms/${vm_id}/ports/8080")"
if [[ "$(api_code "${raw}")" != "204" ]]; then
	echo "Expected HTTP 204 on delete, got: ${raw}"
	exit 1
fi
ports="$(api_get "/api/vms/${vm_id}/ports")"
if [[ "$(jq -r '[.[].port] | sort | join(",")' <<<"${ports}")" != "3000,9090" ]]; then
	echo "Expected ports 3000,9090 after delete, got: ${ports}"
	exit 1
fi

# Deleting a port that is not exposed is a 404.
raw="$(api_raw DELETE "/api/vms/${vm_id}/ports/8080")"
expect_error_code "${raw}" 404

# Port operations on an unknown VM are a 404 too.
raw="$(api_raw GET "/api/vms/vm-123-nonexistent/ports")"
expect_error_code "${raw}" 404

echo "Exposed ports API behaves as expected."
