#!/bin/bash
#
# Build assets/postgrest-rootfs.ext4: the Ubuntu 24.04 base rootfs plus
# PostgREST (static binary from the official GitHub release), producing a
# REST API front for the project's tiko postgres. All the real work lives in
# common.sh.
#
# The webapp provisions these VMs as database APIs: it writes a per-VM
# postgrest.conf pointing at the project's tiko postgres guest IP and runs
# the binary as a systemd service (see webapp/server/src/postgrest.ts), so
# this image only needs the binary itself.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=scripts/rootfs/common.sh
source "${SCRIPT_DIR}/common.sh"

PGRST_VERSION="v14.17"

# Host-side hook invoked by build_rootfs with the image mounted: download the
# static PostgREST binary into /usr/local/bin. Fully static (no libpq needed).
extra_setup() {
	local rootfs="$1"
	local tarball="postgrest-${PGRST_VERSION}-linux-static-x86-64.tar.xz"
	curl -fsSL "https://github.com/PostgREST/postgrest/releases/download/${PGRST_VERSION}/${tarball}" \
		-o "/tmp/${tarball}"
	sudo tar -xJf "/tmp/${tarball}" -C "${rootfs}/usr/local/bin"
	rm -f "/tmp/${tarball}"
}

build_rootfs "postgrest-rootfs.ext4" "" "https://archive.ubuntu.com/ubuntu" \
	postgrest --version
