#!/bin/bash
#
# Proxy: the JWT-authenticated HTTP reverse proxy for exposed guest ports.
# Boots a VM, runs an HTTP server in it, then exercises the full flow: mint
# token -> proxy request with/without token, revocation when the port is
# unexposed, and token expiry.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=tests/common.sh
source "${SCRIPT_DIR}/common.sh"

trap 'cleanup_vms; stop_hostd' EXIT

start_hostd

# The python-3.12 image ships python3, so no in-guest setup is needed.
VM_ID="$(create_vm "proxy-vm" 123 "python-3.12")"
echo "Created VM: ${VM_ID}"

wait_serial_boot "${VM_ID}"
echo "VM ${VM_ID} booted to a login prompt"

# Seed a file to fetch.
api_post "/api/vms/${VM_ID}/exec" \
	'{"cmd":["sh","-c","echo hello-from-guest > /tmp/hello.txt"],"env":[],"cwd":null}' >/dev/null

# Expose guest port 8080 and serve /tmp on it.
api_post "/api/vms/${VM_ID}/ports" '{"port": 8080, "label": "web"}' >/dev/null
WL_CREATE="$(api_post "/api/vms/${VM_ID}/workloads" \
	'{"cmd":["python3","-m","http.server","8080","--directory","/tmp"],"env":[],"cwd":null}')"
WL_ID="$(jq -r '.workload_id' <<<"${WL_CREATE}")"
wait_workload_state "${VM_ID}" "${WL_ID}" "running" 20
echo "HTTP server workload ${WL_ID} running in guest"

# Minting a token for a port that is not exposed fails with 404.
raw="$(api_raw POST "/api/vms/${VM_ID}/ports/9999/token" '{}')"
expect_error_code "${raw}" 404

# Mint a token for the exposed port.
TOKEN_RESP="$(api_post "/api/vms/${VM_ID}/ports/8080/token" '{"ttl_secs": 300}')"
TOKEN="$(jq -r '.token' <<<"${TOKEN_RESP}")"
if [[ -z "${TOKEN}" || "${TOKEN}" == "null" ]]; then
	echo "Failed to mint token: ${TOKEN_RESP}"
	exit 1
fi
echo "Minted proxy token (expires $(jq -r '.expires_at' <<<"${TOKEN_RESP}"))"

# Without a token the proxy rejects the request with the uniform 401 body.
raw="$(curl -sS -w '\n%{http_code}' "${PROXY_URL}/hello.txt")"
expect_error_code "${raw}" 401
echo "Request without token rejected with 401"

# With the token the request is forwarded to the guest server. The workload
# state "running" only means guestd spawned the process, so poll until the
# guest server has actually bound its socket.
body=""
for _ in {1..30}; do
	if body="$(curl -fsS -H "Authorization: Bearer ${TOKEN}" "${PROXY_URL}/hello.txt" 2>/dev/null)"; then
		break
	fi
	sleep 0.5
done
if [[ "${body}" != "hello-from-guest" ]]; then
	echo "Unexpected proxied body: ${body}"
	exit 1
fi
echo "Proxied GET /hello.txt returned the guest file"

# Path passthrough: the directory listing mentions hello.txt.
listing="$(curl -fsS -H "Authorization: Bearer ${TOKEN}" "${PROXY_URL}/")"
if ! grep -q "hello.txt" <<<"${listing}"; then
	echo "Directory listing missing hello.txt: ${listing}"
	exit 1
fi
echo "Proxied GET / returned the guest directory listing"

# Removing the exposed port revokes access immediately, even though the
# token has not expired.
api_raw DELETE "/api/vms/${VM_ID}/ports/8080" >/dev/null
raw="$(curl -sS -w '\n%{http_code}' -H "Authorization: Bearer ${TOKEN}" "${PROXY_URL}/hello.txt")"
expect_error_code "${raw}" 403
echo "After unexposing the port, the same token gets 403"

# Expired tokens are rejected: mint with a 1s TTL and let it lapse.
api_post "/api/vms/${VM_ID}/ports" '{"port": 8080, "label": "web"}' >/dev/null
SHORT_TOKEN="$(api_post "/api/vms/${VM_ID}/ports/8080/token" '{"ttl_secs": 1}' | jq -r '.token')"
sleep 2
raw="$(curl -sS -w '\n%{http_code}' -H "Authorization: Bearer ${SHORT_TOKEN}" "${PROXY_URL}/hello.txt")"
expect_error_code "${raw}" 401
echo "Expired token rejected with 401"

printf '\nProxy test passed. ✅\n\n'
