#!/bin/bash
#
# S3 Files image: a VM created with image "s3files" boots the
# s3files-rootfs.ext4 image (amazon-efs-utils + botocore, credentials and
# file-system config baked in at image build time — see
# scripts/rootfs/build_rootfs_s3files.sh) and must auto-mount the configured
# S3 file system at /mnt/s3files (NFSv4 over the efs-utils TLS tunnel,
# IAM-signed) by the time the system is up. The mount must be read/writable.
#
# Unlike the other tests this needs real AWS resources: the image only
# exists if someone built it from a filled-in scripts/rootfs/s3files-config.
# Skip (pass) when the image has not been built.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=tests/common.sh
source "${SCRIPT_DIR}/common.sh"

if [[ ! -f "${REPO_ROOT}/assets/s3files-rootfs.ext4" ]]; then
	echo "assets/s3files-rootfs.ext4 not built (needs scripts/rootfs/s3files-config) — skipping"
	exit 0
fi

trap 'cleanup_vms; stop_hostd' EXIT

start_hostd

VM_ID="$(create_vm "e2e-s3files" 123 s3files)"
echo "Created VM: ${VM_ID}"

wait_serial_boot "${VM_ID}"
echo "VM ${VM_ID} booted to a login prompt"

# The mount unit is ordered after network.target and needs the TLS tunnel
# plus the IAM-signed mount handshake, so it can lag the login prompt a bit.
MOUNTED=0
for _ in $(seq 1 36); do
	EXEC_RESPONSE="$(api_post "/api/vms/${VM_ID}/exec" \
		'{"cmd":["mountpoint","-q","/mnt/s3files"],"env":[],"cwd":null}' 2>/dev/null || true)"
	if [[ "$(jq -r '.exit_code // 1' <<<"${EXEC_RESPONSE}")" == "0" ]]; then
		MOUNTED=1
		break
	fi
	sleep 5
done
if [[ "${MOUNTED}" -ne 1 ]]; then
	echo "/mnt/s3files never mounted; unit diagnostics:"
	api_post "/api/vms/${VM_ID}/exec" \
		'{"cmd":["sh","-c","systemctl status mnt-s3files.mount --no-pager -l; journalctl -u mnt-s3files.mount --no-pager | tail -30"],"env":[],"cwd":null}' || true
	exit 1
fi
echo "/mnt/s3files is mounted"

# It must be an NFS mount served through the local efs-utils TLS tunnel.
EXEC_RESPONSE="$(api_post "/api/vms/${VM_ID}/exec" \
	'{"cmd":["findmnt","-n","-o","FSTYPE","/mnt/s3files"],"env":[],"cwd":null}')"
if [[ "$(jq -r '.exit_code' <<<"${EXEC_RESPONSE}")" != "0" ]] \
	|| ! jq -e '.logs[] | select(.stream == "stdout") | .data | select(contains("nfs"))' \
		<<<"${EXEC_RESPONSE}" >/dev/null; then
	echo "Expected an nfs mount at /mnt/s3files: ${EXEC_RESPONSE}"
	exit 1
fi
echo "/mnt/s3files is an NFS mount"

# Write + read back + delete through the guest mount (this round-trips to S3).
MARKER="tikovm-s3files-e2e-$$"
EXEC_RESPONSE="$(api_post "/api/vms/${VM_ID}/exec" \
	"{\"cmd\":[\"sh\",\"-c\",\"echo ${MARKER} > /mnt/s3files/.tikovm-e2e-probe && cat /mnt/s3files/.tikovm-e2e-probe && rm /mnt/s3files/.tikovm-e2e-probe\"],\"env\":[],\"cwd\":null}")"
if [[ "$(jq -r '.exit_code' <<<"${EXEC_RESPONSE}")" != "0" ]] \
	|| ! jq -e --arg m "${MARKER}" '.logs[] | select(.stream == "stdout") | .data | select(contains($m))' \
		<<<"${EXEC_RESPONSE}" >/dev/null; then
	echo "Write/read/delete through /mnt/s3files failed: ${EXEC_RESPONSE}"
	exit 1
fi
echo "Guest write + read-back + delete through /mnt/s3files succeeded"

printf '\nS3 Files test passed. ✅\n\n'
