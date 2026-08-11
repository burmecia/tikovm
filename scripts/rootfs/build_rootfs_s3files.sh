#!/bin/bash
#
# Build assets/s3files-rootfs.ext4: the Ubuntu 24.04 base rootfs plus AWS S3
# Files support (https://docs.aws.amazon.com/AmazonS3/latest/userguide/s3-files.html).
# All the generic image work lives in common.sh.
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

CONFIG_FILE="${SCRIPT_DIR}/s3files-config"

if [[ ! -f "${CONFIG_FILE}" ]]; then
	echo "ERROR: ${CONFIG_FILE} not found." >&2
	echo "Copy ${SCRIPT_DIR}/s3files-config.sample to s3files-config and fill in real values." >&2
	exit 1
fi

# shellcheck source=/dev/null
source "${CONFIG_FILE}"

for var in FILE_SYSTEM_ID MOUNT_TARGET_IP AWS_REGION AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY; do
	if [[ -z "${!var:-}" ]]; then
		echo "ERROR: ${var} is not set in ${CONFIG_FILE}." >&2
		exit 1
	fi
done

# Host-side hook invoked by build_rootfs with the image mounted.
extra_setup() {
	local rootfs="$1"

	# Stage the config inside the image (0600 — it carries credentials) plus
	# the chroot script that consumes it; both delete themselves at the end.
	sudo install -m 0600 "${CONFIG_FILE}" "${rootfs}/tmp/s3files-config"
	sudo tee "${rootfs}/tmp/tikovm-s3files-setup.sh" > /dev/null << 'EOF'
#!/bin/bash
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive

source /tmp/s3files-config

# amazon-efs-utils: the installer adds AWS's apt repo and installs the
# package (mount.efs, stunnel wiring, the mount watchdog).
curl -fsSL https://amazon-efs-utils.aws.com/efs-utils-installer.sh | sh -s -- --install

# botocore: used by mount.efs to SigV4-sign the mount. Noble marks the
# system Python externally-managed (PEP 668), hence --break-system-packages.
pip3 install --break-system-packages --target /usr/lib/python3/dist-packages botocore >/dev/null 2>&1

# Credentials for the default AWS credential chain (mount.efs runs as root).
mkdir -p /root/.aws
chmod 0700 /root/.aws
{
	echo "[default]"
	echo "aws_access_key_id = ${AWS_ACCESS_KEY_ID}"
	echo "aws_secret_access_key = ${AWS_SECRET_ACCESS_KEY}"
	if [[ -n "${AWS_SESSION_TOKEN:-}" ]]; then
		echo "aws_session_token = ${AWS_SESSION_TOKEN}"
	fi
} > /root/.aws/credentials
chmod 0600 /root/.aws/credentials
{
	echo "[default]"
	echo "region = ${AWS_REGION}"
} > /root/.aws/config
chmod 0600 /root/.aws/config

# Region for mount.efs itself.
if [[ -f /etc/amazon/efs/efs-utils.conf ]]; then
	sed -i -E "s/^#?region = .*/region = ${AWS_REGION}/" /etc/amazon/efs/efs-utils.conf
	grep -q '^region = ' /etc/amazon/efs/efs-utils.conf \
		|| sed -i "/^\[mount\]/a region = ${AWS_REGION}" /etc/amazon/efs/efs-utils.conf
fi

# Auto-mount at boot. mounttargetip pins the mount to the given mount target
# (no DNS for <fs-id>.efs.<region>.amazonaws.com exists inside the guest's
# VPC-less networking); tls + iam = TLS tunnel + SigV4-signed mount. _netdev
# Auto-mount at boot. mounttargetip pins the mount to the given mount target
# (no DNS for <fs-id>.efs.<region>.amazonaws.com exists inside the guest's
# VPC-less networking); tls + iam = TLS tunnel + SigV4-signed mount.
#
# Type MUST be s3files, not efs: efs-utils 3.2 ships a dedicated
# /sbin/mount.s3files helper for S3 file systems. Mount targets of an S3
# file system present a TLS cert for *.<region>.s3files.on.aws; the efs
# helper builds checkHost=<fs-id>.efs.<region>.amazonaws.com, which can never
# match, and the TLS tunnel stalls (the NFS mount through it then times out).
# mount.s3files builds checkHost=<fs-id>.s3files.<region>.on.aws instead —
# this is exactly how the production host mounts the same file system
# (see its /etc/fstab entry: type s3files, same options).
#
# Ordering notes (both earned the hard way):
# - DefaultDependencies=no: without _netdev systemd treats the unit as a
#   LOCAL mount and force-orders it Before=local-fs.target, which contradicts
#   After=network.target — systemd breaks the resulting ordering cycle at
#   random, so boot ordering flips between runs. With default deps off the
#   explicit ordering below is the only one.
# - network-online.target must stay unpulled (no _netdev, no
#   Wants=network-online.target): see the wait-online mask below.
mkdir -p /mnt/s3files
cat > /etc/systemd/system/mnt-s3files.mount << UNIT
[Unit]
Description=AWS S3 Files (${FILE_SYSTEM_ID}) at /mnt/s3files
DefaultDependencies=no
After=network.target
Before=remote-fs.target

[Mount]
What=${FILE_SYSTEM_ID}:/
Where=/mnt/s3files
Type=s3files
Options=tls,iam,mounttargetip=${MOUNT_TARGET_IP}

[Install]
WantedBy=remote-fs.target
UNIT
mkdir -p /etc/systemd/system/remote-fs.target.wants
ln -sf /etc/systemd/system/mnt-s3files.mount \
	/etc/systemd/system/remote-fs.target.wants/mnt-s3files.mount

# Mask systemd-networkd-wait-online: nfs-common's rpc-statd-notify.service
# Wants network-online.target, which runs wait-online — but udev renames the
# NIC to enp0s3, which no .network file matches, so wait-online hangs until
# its timeout and delays the whole boot by minutes. It provides no value in
# these guests regardless: the guest IP comes from the kernel ip= boot
# argument, so the NIC is configured seconds before systemd starts.
systemctl mask systemd-networkd-wait-online.service

# Monitor TLS mount health at boot.
systemctl enable amazon-efs-mount-watchdog 2>/dev/null || true

# Never leave the credentials copy behind.
rm -f /tmp/s3files-config /tmp/tikovm-s3files-setup.sh
EOF

	# Same bind/umount pattern as build_rootfs_postgres16.sh: unmount even
	# when the chroot fails, so a failed build never leaves bind mounts (and
	# a mounted image) behind.
	sudo mount --bind /proc "${rootfs}/proc"
	sudo mount --bind /dev "${rootfs}/dev"
	sudo mount --bind /dev/pts "${rootfs}/dev/pts"
	local status=0
	sudo chroot "${rootfs}" bash /tmp/tikovm-s3files-setup.sh || status=$?
	sudo umount "${rootfs}/dev/pts"
	sudo umount "${rootfs}/dev"
	sudo umount "${rootfs}/proc"
	# Belt-and-braces: if the chroot died before its own cleanup, scrub the
	# credentials copy from the image before returning the failure.
	if [[ "${status}" -ne 0 ]]; then
		sudo rm -f "${rootfs}/tmp/s3files-config"
	fi
	return "${status}"
}

build_rootfs "s3files-rootfs.ext4" "python3,python3-pip,nfs-common,stunnel4" \
	"https://archive.ubuntu.com/ubuntu" \
	bash -c 'test -x /sbin/mount.s3files && python3 -c "import botocore"'
