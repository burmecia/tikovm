#!/bin/bash
#
# Build assets/python-3.12-rootfs.ext4: the Ubuntu 24.04 base rootfs plus
# Python 3.12 (noble's stock `python3` package). All the real work lives in
# common.sh.
#
# Uses the https apt mirror because this host's egress blocks plain http/80.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=scripts/rootfs/common.sh
source "${SCRIPT_DIR}/common.sh"

build_rootfs "python-3.12-rootfs.ext4" "python3" "https://archive.ubuntu.com/ubuntu" \
	python3 --version
