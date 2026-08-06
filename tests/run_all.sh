#!/bin/bash
#
# Run the full end-to-end suite: each test_*.sh file is self-contained (it
# starts its own hostd and creates its own VMs) and can also be run on its
# own. Fail-fast on the first failing file, same as the old monolithic
# test_e2e.sh.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

TESTS=(
	test_vm_lifecycle.sh
	test_pause_resume.sh
	test_snapshot_restore.sh
	test_workloads.sh
	test_exec.sh
	test_networking.sh
	test_ports.sh
	test_proxy.sh
	test_auto_suspend.sh
)

for t in "${TESTS[@]}"; do
	echo "=== ${t} ==="
	if "${SCRIPT_DIR}/${t}"; then
		echo "=== ${t}: PASS ==="
	else
		echo "=== ${t}: FAIL ==="
		exit 1
	fi
done

printf '\nall e2e tests passed. ✅\n\n'
