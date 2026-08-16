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
# the Node.js tarball and unpack it into the image's /usr/local, then bake
# the `pg` driver into /opt/lambda so webapp lambda VMs can talk to their
# project's tiko postgres without an npm install at provision time.
extra_setup() {
	local rootfs="$1"
	local tarball="node-${NODE_VERSION}-linux-x64.tar.xz"
	curl -fsSL "https://nodejs.org/dist/${NODE_VERSION}/${tarball}" -o "/tmp/${tarball}"
	sudo tar -xJf "/tmp/${tarball}" -C "${rootfs}/usr/local" --strip-components=1
	rm -f "/tmp/${tarball}"

	# npm must run with the image as root (its shebang resolves node);
	# chroot shares the host's network and the image's resolv.conf is
	# already set, so the https registry is reachable. Same bind/umount
	# discipline as setup_s3files: unmount even when the chroot fails.
	sudo mkdir -p "${rootfs}/opt/lambda"
	sudo mount --bind /proc "${rootfs}/proc"
	sudo mount --bind /dev "${rootfs}/dev"
	sudo mount --bind /dev/pts "${rootfs}/dev/pts"
	local status=0
	sudo chroot "${rootfs}" /usr/local/bin/npm install --prefix /opt/lambda \
		--no-audit --no-fund pg || status=$?
	sudo umount "${rootfs}/dev/pts"
	sudo umount "${rootfs}/dev"
	sudo umount "${rootfs}/proc"
	return "${status}"
}

# The verify cmd doubles as the pg-driver assert: it fails the build if the
# baked-in lambda driver is not loadable.
build_rootfs "node-22-rootfs.ext4" "" "https://archive.ubuntu.com/ubuntu" \
	node -e "console.log(process.version); require('/opt/lambda/node_modules/pg')"
