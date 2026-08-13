#!/bin/bash
#
# Build assets/s3files-rootfs.ext4: the Ubuntu 24.04 base rootfs plus AWS S3
# Files support (https://docs.aws.amazon.com/AmazonS3/latest/userguide/s3-files.html).
# All the generic image work lives in common.sh; the S3 Files-specific setup
# is the shared setup_s3files helper in common.sh, so other images can layer
# software on top of the same S3 mount (see build_rootfs_tiko_postgres.sh).
#
# S3 Files presents an S3 bucket as an NFSv4 file system; it is mounted with
# amazon-efs-utils' dedicated S3 Files helper (mount.s3files), which wraps
# the NFS mount in a local TLS tunnel (efs-proxy) and signs the mount with
# SigV4 (that is what botocore is needed for). The image auto-mounts the file
# system at /mnt/s3files at boot via a baked-in systemd mount unit.
#
# The file system ID, mount target IP, region, and AWS credentials are NOT
# committed: they are read at build time from `s3files-config` (git-ignored)
# next to this script — copy `s3files-config.sample` and fill it in. The
# values are baked into the image (mount unit, efs-utils.conf, and
# /root/.aws/credentials mode 0600); the image artifact itself is git-ignored
# under /assets/*.
#
# Uses the https apt mirror because this host's egress blocks plain http/80.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=scripts/rootfs/common.sh
source "${SCRIPT_DIR}/common.sh"

# Host-side hook invoked by build_rootfs with the image mounted; all the S3
# Files work lives in the shared setup_s3files helper.
extra_setup() {
	setup_s3files "$1"
}

build_rootfs "s3files-rootfs.ext4" "python3,python3-pip,nfs-common,stunnel4" \
	"https://archive.ubuntu.com/ubuntu" \
	bash -c 'test -x /sbin/mount.s3files && python3 -c "import botocore"'
