#!/bin/bash
#
# Proxy TCP mode: the Postgres wire protocol over the proxy listener.
# Boots a postgres-16 VM, exposes port 5432, then exercises the full flow:
# mint a proto=tcp token -> psql through the proxy with the token in the
# tikovm_token startup parameter, rejection without a token, rejection of an
# http-proto token, and revocation when the port is unexposed.
#
# Needs psql on the host; postgresql-client is apt-installed if missing.

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

VM_ID="$(create_vm "proxy-tcp-vm" 123 "postgres-16")"
echo "Created VM: ${VM_ID}"

wait_serial_boot "${VM_ID}"
echo "VM ${VM_ID} booted to a login prompt"

# Expose the PostgreSQL port and mint a tcp-proto token for it.
api_post "/api/vms/${VM_ID}/ports" '{"port": 5432, "label": "postgres"}' >/dev/null
TOKEN_RESP="$(api_post "/api/vms/${VM_ID}/ports/5432/token" '{"ttl_secs": 300, "proto": "tcp"}')"
TOKEN="$(jq -r '.token' <<<"${TOKEN_RESP}")"
if [[ -z "${TOKEN}" || "${TOKEN}" == "null" ]]; then
	echo "Failed to mint tcp token: ${TOKEN_RESP}"
	exit 1
fi
echo "Minted tcp proxy token"

# psql through the proxy. The JWT rides in the tikovm_token startup
# parameter; libpq turns PGOPTIONS `-c name=value` into startup parameters.
# The guest cluster accepts postgres/postgres from the project subnet (the
# host reaches the guest over the bridge). sslmode defaults to prefer, so
# this also exercises the proxy's SSLRequest -> 'N' negotiation.
export PGPASSWORD=postgres
psql_proxy() { # psql_proxy <token|"-"> [psql args...]
	local token="$1"
	shift
	local -a opts=()
	if [[ "${token}" != "-" ]]; then
		opts=("PGOPTIONS=-c tikovm_token=${token}")
	fi
	env "${opts[@]}" \
		psql "host=127.0.0.1 port=${PROXY_ADDR##*:} user=postgres dbname=postgres connect_timeout=10" \
		"$@"
}

# The workload-free VM reports "booted" before postgresql.service is up, so
# poll until a proxied query succeeds.
ok=0
for _ in {1..60}; do
	if psql_proxy "${TOKEN}" -tAc 'select 1' 2>/dev/null | grep -q '^1$'; then
		ok=1
		break
	fi
	sleep 1
done
if [[ "${ok}" -ne 1 ]]; then
	echo "proxied psql 'select 1' did not succeed"
	psql_proxy "${TOKEN}" -tAc 'select 1' || true
	exit 1
fi
echo "psql through the proxy returned select 1"

# Without the token parameter the proxy rejects the startup with a FATAL
# ErrorResponse naming the missing parameter.
if psql_proxy - -tAc 'select 1' >/dev/null 2>&1; then
	echo "proxied psql without token unexpectedly succeeded"
	exit 1
fi
err="$(psql_proxy - -tAc 'select 1' 2>&1 || true)"
if ! grep -q "tikovm_token" <<<"${err}"; then
	echo "expected an error naming tikovm_token, got: ${err}"
	exit 1
fi
echo "Connection without token rejected: $(grep -o 'FATAL.*' <<<"${err}" | head -1)"

# An http-proto token must not work for TCP proxying.
HTTP_TOKEN="$(api_post "/api/vms/${VM_ID}/ports/5432/token" '{"ttl_secs": 300}' | jq -r '.token')"
err="$(psql_proxy "${HTTP_TOKEN}" -tAc 'select 1' 2>&1 || true)"
if ! grep -q "not valid for TCP" <<<"${err}"; then
	echo "expected a 'not valid for TCP' error, got: ${err}"
	exit 1
fi
echo "http-proto token rejected for TCP proxying"

# Unexposing the port revokes access immediately, token still valid.
api_raw DELETE "/api/vms/${VM_ID}/ports/5432" >/dev/null
err="$(psql_proxy "${TOKEN}" -tAc 'select 1' 2>&1 || true)"
if ! grep -q "no longer exposed" <<<"${err}"; then
	echo "expected a 'no longer exposed' error, got: ${err}"
	exit 1
fi
echo "After unexposing the port, the same token is rejected"

printf '\nProxy TCP test passed. ✅\n\n'
