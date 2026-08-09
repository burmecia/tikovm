#!/bin/bash
#
# Block storage: a VM created with `block_storage` gets a dedicated ublk
# block device (/dev/vdc) backed by chunk files under the storage root,
# ext4-formatted by hostd and mounted in the guest at /mnt/tikovm-data by a
# seeded systemd unit. Data must survive snapshot/restore (the volume is
# independent of the Firecracker process), and the volume directory must be
# removed when the VM is destroyed. Invalid configs are rejected at create
# time.
#
# The storage root is a plain local directory here: the chunk store is
# ordinary file IO, so the test needs no S3 Files mount.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=tests/common.sh
source "${SCRIPT_DIR}/common.sh"

trap 'cleanup_vms; stop_hostd' EXIT

STORAGE_ROOT="$(mktemp -d -t tikovm-storage-e2e.XXXXXX)"
start_hostd --storage-root "${STORAGE_ROOT}"

# --- invalid configs are rejected with the uniform JSON error ---------------
BAD_SIZE_RAW="$(api_raw POST /api/vms '{
	"name": "e2e-bs-bad", "project_id": 123, "mode": "ephemeral", "image": "ubuntu-24",
	"cpus": 1, "memory_mb": 512, "disk_size_mb": 1024,
	"network_config": {"allow_internet": true, "exposed_ports": [], "egress": [], "public_access": false},
	"ssh_access": false, "env": [], "cmd": [], "services": [], "cron_schedule": null, "tags": [],
	"block_storage": {"size_mb": 32}
}')"
expect_error_code "${BAD_SIZE_RAW}" "500"
echo "size_mb < 128 rejected: $(api_body "${BAD_SIZE_RAW}")"

BAD_CHUNK_RAW="$(api_raw POST /api/vms '{
	"name": "e2e-bs-bad2", "project_id": 123, "mode": "ephemeral", "image": "ubuntu-24",
	"cpus": 1, "memory_mb": 512, "disk_size_mb": 1024,
	"network_config": {"allow_internet": true, "exposed_ports": [], "egress": [], "public_access": false},
	"ssh_access": false, "env": [], "cmd": [], "services": [], "cron_schedule": null, "tags": [],
	"block_storage": {"size_mb": 512, "chunk_kb": 300}
}')"
expect_error_code "${BAD_CHUNK_RAW}" "500"
echo "bad chunk_kb rejected: $(api_body "${BAD_CHUNK_RAW}")"

# --- VM with a 512 MiB block volume ------------------------------------------
CREATE_RESPONSE="$(api_post /api/vms '{
	"name": "e2e-bs", "project_id": 123, "mode": "ephemeral", "image": "ubuntu-24",
	"cpus": 1, "memory_mb": 512, "disk_size_mb": 1024,
	"network_config": {"allow_internet": true, "exposed_ports": [], "egress": [], "public_access": false},
	"ssh_access": false, "env": [], "cmd": [], "services": [], "cron_schedule": null, "tags": [],
	"block_storage": {"size_mb": 512}
}')"
VM_ID="$(jq -r '.id' <<<"${CREATE_RESPONSE}")"
if [[ -z "${VM_ID}" || "${VM_ID}" == "null" ]]; then
	echo "Failed to create block-storage VM: ${CREATE_RESPONSE}"
	exit 1
fi
register_vm "${VM_ID}"
echo "Created VM with block volume: ${VM_ID}"

# The chunk tree must exist under the storage root for this VM.
VOLUME_DIR="${STORAGE_ROOT}/proj-123/${VM_ID}"
if [[ ! -f "${VOLUME_DIR}/meta.json" ]]; then
	echo "Volume meta.json missing at ${VOLUME_DIR}"
	ls -laR "${STORAGE_ROOT}" || true
	exit 1
fi
jq -e '.size_bytes == 536870912 and .chunk_size == 1048576' "${VOLUME_DIR}/meta.json" >/dev/null || {
	echo "Unexpected meta.json: $(cat "${VOLUME_DIR}")"
	exit 1
}
echo "Volume directory with meta.json exists at ${VOLUME_DIR}"

wait_serial_boot "${VM_ID}"

# The data device must be attached and auto-mounted by the seeded unit.
EXEC_RESPONSE="$(api_post "/api/vms/${VM_ID}/exec" \
	'{"cmd":["sh","-c","test -b /dev/vdc && findmnt -n -o FSTYPE,SOURCE /mnt/tikovm-data"],"env":[],"cwd":null}')"
if [[ "$(jq -r '.exit_code' <<<"${EXEC_RESPONSE}")" != "0" ]]; then
	echo "/dev/vdc missing or not mounted at /mnt/tikovm-data: ${EXEC_RESPONSE}"
	exit 1
fi
jq -e '.logs[] | select(.stream == "stdout") | .data | select(contains("ext4"))' \
	<<<"${EXEC_RESPONSE}" >/dev/null || {
	echo "Expected ext4 mount at /mnt/tikovm-data: ${EXEC_RESPONSE}"
	exit 1
}
echo "/dev/vdc attached and mounted ext4 at /mnt/tikovm-data"

# Write + fsync + read back through the guest mount.
EXEC_RESPONSE="$(api_post "/api/vms/${VM_ID}/exec" \
	'{"cmd":["sh","-c","echo tikovm-block-e2e > /mnt/tikovm-data/probe.txt && sync && cat /mnt/tikovm-data/probe.txt"],"env":[],"cwd":null}')"
if [[ "$(jq -r '.exit_code' <<<"${EXEC_RESPONSE}")" != "0" ]] \
	|| ! jq -e '.logs[] | select(.stream == "stdout") | .data | select(contains("tikovm-block-e2e"))' \
		<<<"${EXEC_RESPONSE}" >/dev/null; then
	echo "Write/read through /mnt/tikovm-data failed: ${EXEC_RESPONSE}"
	exit 1
fi
echo "Guest write + sync + read-back succeeded"

# The write must have materialized chunk files on the storage root.
CHUNK_COUNT="$(sudo find "${VOLUME_DIR}/chunks" -type f | wc -l)"
if [[ "${CHUNK_COUNT}" -eq 0 ]]; then
	echo "No chunk files under ${VOLUME_DIR}/chunks after guest write"
	exit 1
fi
echo "${CHUNK_COUNT} chunk files materialized on the storage root"

# --- snapshot/restore keeps the volume ----------------------------------------
api_post "/api/vms/${VM_ID}/snapshot" >/dev/null
RESTORE_RESPONSE="$(api_post "/api/vms/${VM_ID}/restore")"
if [[ "$(jq -r '.state' <<<"${RESTORE_RESPONSE}")" != "started" ]]; then
	echo "Unexpected restore response: ${RESTORE_RESPONSE}"
	exit 1
fi
expect_fc_state "${VM_ID}" "Running"

EXEC_RESPONSE="$(api_post "/api/vms/${VM_ID}/exec" \
	'{"cmd":["cat","/mnt/tikovm-data/probe.txt"],"env":[],"cwd":null}')"
if [[ "$(jq -r '.exit_code' <<<"${EXEC_RESPONSE}")" != "0" ]] \
	|| ! jq -e '.logs[] | select(.stream == "stdout") | .data | select(contains("tikovm-block-e2e"))' \
		<<<"${EXEC_RESPONSE}" >/dev/null; then
	echo "probe.txt did not survive snapshot/restore: ${EXEC_RESPONSE}"
	exit 1
fi
echo "Volume data survived snapshot/restore"

# --- destroy removes the volume ------------------------------------------------
api_raw DELETE "/api/vms/${VM_ID}" >/dev/null
for _ in {1..50}; do
	[[ ! -d "${VOLUME_DIR}" ]] && break
	sleep 0.2
done
if [[ -d "${VOLUME_DIR}" ]]; then
	echo "Volume directory ${VOLUME_DIR} still exists after VM destroy"
	exit 1
fi
echo "Volume directory removed on VM destroy"

printf '\nBlock storage test passed. ✅\n\n'
