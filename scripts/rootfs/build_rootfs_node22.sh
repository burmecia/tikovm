#!/bin/bash
#
# Build assets/node-22-rootfs.ext4: the Ubuntu 24.04 base rootfs plus
# Node.js 22 (LTS). All the real work lives in common.sh.
#
# Noble's own `nodejs` package is Node 18, so Node 22 comes from the
# official tarball at nodejs.org, unpacked into /usr/local of the image
# (bin/node, bin/npm, ...). Uses the https apt mirror because this host's
# egress blocks plain http/80.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=scripts/rootfs/common.sh
source "${SCRIPT_DIR}/common.sh"

NODE_VERSION="v22.23.2"

# Host-side hook invoked by build_rootfs with the image mounted: download
# the Node.js tarball and unpack it into the image's /usr/local.
extra_setup() {
	local rootfs="$1"
	local tarball="node-${NODE_VERSION}-linux-x64.tar.xz"
	curl -fsSL "https://nodejs.org/dist/${NODE_VERSION}/${tarball}" -o "/tmp/${tarball}"
	sudo tar -xJf "/tmp/${tarball}" -C "${rootfs}/usr/local" --strip-components=1
	rm -f "/tmp/${tarball}"
}

build_rootfs "node-22-rootfs.ext4" "" "https://archive.ubuntu.com/ubuntu" \
	node --version
