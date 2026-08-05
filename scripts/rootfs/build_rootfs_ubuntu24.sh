#!/bin/bash
#
# Build assets/ubuntu-24.04-rootfs.ext4: the base guest image (Ubuntu 24.04
# minbase + systemd + guestd). All the real work lives in common.sh.
#
# Note: debootstrap pulls from the plain http mirror here — on hosts whose
# egress blocks http/80, build via an https mirror instead (see
# build_rootfs_python312.sh).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=scripts/rootfs/common.sh
source "${SCRIPT_DIR}/common.sh"

build_rootfs "ubuntu-24.04-rootfs.ext4" "" "http://archive.ubuntu.com/ubuntu"
