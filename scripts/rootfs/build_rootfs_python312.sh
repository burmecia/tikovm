#!/bin/bash
#
# Build assets/python-3.12-rootfs.ext4: the Ubuntu 24.04 base rootfs plus
# Python 3.12 (noble's stock `python3` package). All the real work lives in
# common.sh.
#
# python3-psycopg2 is baked in so webapp lambda VMs can connect to their
# project's tiko postgres without an apt install at provision time.
#
# Uses the https apt mirror because this host's egress blocks plain http/80.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=scripts/rootfs/common.sh
source "${SCRIPT_DIR}/common.sh"

# The verify cmd doubles as the psycopg2 assert: it fails the build if the
# baked-in lambda driver is not importable.
build_rootfs "python-3.12-rootfs.ext4" "python3,python3-psycopg2" "https://archive.ubuntu.com/ubuntu" \
	python3 -c "import psycopg2, sys; print(sys.version)"
