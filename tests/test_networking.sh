#!/bin/bash
#
# Networking: two VMs in the same project must share the project's subnet and
# bridge (`tbr-<project_id>`) with distinct guest IPs and one TAP each; both
# guests must answer ping from the host. Deleting one VM releases only its
# TAP; deleting the last VM tears down the bridge.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=tests/common.sh
source "${SCRIPT_DIR}/common.sh"

trap 'cleanup_vms; stop_hostd' EXIT

start_hostd

VM_ID="$(create_vm "e2e-vm")"
echo "Created VM: ${VM_ID}"

NET1_RESPONSE="$(api_get "/api/vms/${VM_ID}")"
VM1_SUBNET="$(jq -r '.net.subnet' <<<"${NET1_RESPONSE}")"
VM1_GUEST_IP="$(jq -r '.net.guest_ip' <<<"${NET1_RESPONSE}")"
VM1_TAP="$(jq -r '.net.tap_name' <<<"${NET1_RESPONSE}")"
if [[ -z "${VM1_SUBNET}" || "${VM1_SUBNET}" == "null" ]]; then
	echo "VM ${VM_ID} has no network allocation: ${NET1_RESPONSE}"
	exit 1
fi

# A second VM in the same project must land in the same subnet, on the same
# per-project bridge, with a distinct guest IP.
VM2_ID="$(create_vm "e2e-vm-2")"
echo "Created VM: ${VM2_ID}"

NET2_RESPONSE="$(api_get "/api/vms/${VM2_ID}")"
VM2_SUBNET="$(jq -r '.net.subnet' <<<"${NET2_RESPONSE}")"
VM2_GUEST_IP="$(jq -r '.net.guest_ip' <<<"${NET2_RESPONSE}")"
VM2_TAP="$(jq -r '.net.tap_name' <<<"${NET2_RESPONSE}")"
if [[ "${VM2_SUBNET}" != "${VM1_SUBNET}" ]]; then
	echo "VMs of the same project got different subnets: ${VM1_SUBNET} vs ${VM2_SUBNET}"
	exit 1
fi
if [[ "${VM2_GUEST_IP}" == "${VM1_GUEST_IP}" ]]; then
	echo "VMs of the same project got the same guest IP: ${VM1_GUEST_IP}"
	exit 1
fi
echo "Both VMs share subnet ${VM1_SUBNET} (guest IPs ${VM1_GUEST_IP}, ${VM2_GUEST_IP})"

# Host topology: one bridge for the project, one TAP per VM enslaved to it.
BRIDGE="tbr-123"
if ! ip link show "${BRIDGE}" >/dev/null 2>&1; then
	echo "Project bridge ${BRIDGE} does not exist"
	exit 1
fi
for TAP in "${VM1_TAP}" "${VM2_TAP}"; do
	if ! ip link show "${TAP}" >/dev/null 2>&1; then
		echo "TAP device ${TAP} does not exist"
		exit 1
	fi
	TAP_MASTER="$(ip -o link show "${TAP}" | grep -oP 'master \K\S+' || true)"
	if [[ "${TAP_MASTER}" != "${BRIDGE}" ]]; then
		echo "TAP ${TAP} is not enslaved to ${BRIDGE} (master: ${TAP_MASTER:-none})"
		exit 1
	fi
done
echo "Bridge ${BRIDGE} carries TAPs ${VM1_TAP} and ${VM2_TAP}"

# Data path check: both guests answer ping from the host over the bridge.
# The guest IP is delivered as a kernel ip= boot argument, so eth0 is up once
# the guest has booted.
wait_serial_boot "${VM_ID}"
wait_serial_boot "${VM2_ID}"
for IP in "${VM1_GUEST_IP}" "${VM2_GUEST_IP}"; do
	PING_OK=0
	for _ in {1..15}; do
		if ping -c 1 -W 1 "${IP}" >/dev/null 2>&1; then
			PING_OK=1
			break
		fi
		sleep 1
	done
	if [[ "${PING_OK}" -ne 1 ]]; then
		echo "Guest ${IP} does not answer ping from the host"
		exit 1
	fi
done
echo "Both guests answer ping from the host"

# Deleting the second VM must release its TAP but keep the bridge alive
# while the first VM still uses it.
DELETE2_RAW="$(api_raw DELETE "/api/vms/${VM2_ID}")"
DELETE2_CODE="$(api_code "${DELETE2_RAW}")"
if [[ "${DELETE2_CODE}" != "204" ]]; then
	echo "Expected 204 from deleting ${VM2_ID}, got ${DELETE2_CODE}"
	exit 1
fi
if ip link show "${VM2_TAP}" >/dev/null 2>&1; then
	echo "TAP ${VM2_TAP} still exists after deleting ${VM2_ID}"
	exit 1
fi
if ! ip link show "${BRIDGE}" >/dev/null 2>&1; then
	echo "Bridge ${BRIDGE} was torn down while VM ${VM_ID} is still running"
	exit 1
fi
echo "Deleted VM ${VM2_ID}; bridge ${BRIDGE} kept alive for VM ${VM_ID}"

# Delete the first VM, expecting 204 No Content
DELETE_RAW="$(api_raw DELETE "/api/vms/${VM_ID}")"
DELETE_CODE="$(api_code "${DELETE_RAW}")"
if [[ "${DELETE_CODE}" != "204" ]]; then
	echo "Expected 204 from delete, got ${DELETE_CODE}"
	exit 1
fi
echo "Deleted VM ${VM_ID} (HTTP ${DELETE_CODE})"

# With the project's last VM gone, its bridge, TAP and subnet must be torn
# down as well.
if ip link show "${BRIDGE}" >/dev/null 2>&1; then
	echo "Bridge ${BRIDGE} still exists after deleting the project's last VM"
	exit 1
fi
if ip link show "${VM1_TAP}" >/dev/null 2>&1; then
	echo "TAP ${VM1_TAP} still exists after deleting ${VM_ID}"
	exit 1
fi
echo "Bridge ${BRIDGE} and TAP ${VM1_TAP} torn down with the project's last VM"

printf '\nNetworking test passed. ✅\n\n'
