# shellcheck shell=bash
#
# Shared logic for the per-image build_rootfs_*.sh scripts in this directory.
# This file is meant to be sourced, not executed: an entry script sources it
# and calls `build_rootfs` with its image-specific parameters.
#
# Every image is an ext4 file holding an Ubuntu 24.04 (noble) minbase +
# systemd rootfs with the release-built guestd installed as a systemd
# service. The image is the shared read-only lower layer of the overlayfs
# root the initramfs assembles per VM (see scripts/initramfs_init.sh), so it
# must NOT hardcode a network address: each guest's eth0 address comes from
# the kernel `ip=` boot argument hostd passes at VM creation time.

set -euo pipefail

ROOTFS_SCRIPTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${ROOTFS_SCRIPTS_DIR}/../.." && pwd)"
ASSETS_DIR="${REPO_ROOT}/assets"

# Packages every image gets on top of debootstrap --variant=minbase.
BASE_PACKAGES="systemd,systemd-sysv,udev,sudo,iproute2,iputils-ping,curl,vim,openssh-server,ca-certificates,wget"

# build_rootfs <image_name> <extra_packages_csv> <apt_mirror> [verify_cmd...]
#
# Builds assets/<image_name> from scratch. <extra_packages_csv> is appended to
# BASE_PACKAGES in the debootstrap --include list. The optional <verify_cmd>
# is run inside the finished image (via chroot) before the final e2fsck —
# use it to assert the image's raison d'être, e.g. `python3 --version`.
build_rootfs() {
	local image_name="$1" extra_packages="$2" mirror="$3"
	shift 3

	local image="${ASSETS_DIR}/${image_name}"
	# Per-image mountpoint so concurrent builds don't clash.
	local rootfs="/tmp/rootfs-${image_name%.ext4}"
	local include="${BASE_PACKAGES}${extra_packages:+,${extra_packages}}"

	echo ">>> Build guestd..."
	cargo build --release --manifest-path "${REPO_ROOT}/Cargo.toml" -p guestd

	echo ">>> Install debootstrap..."
	sudo apt update -qq
	sudo apt install debootstrap -y >/dev/null 2>&1

	echo ">>> Create and mount the image..."
	ROOTFS_SIZE_MB="${ROOTFS_SIZE_MB:-4096}"
	rm -f "${image}"
	truncate -s "${ROOTFS_SIZE_MB}M" "${image}"
	mkfs.ext4 "${image}"
	mkdir -p "${rootfs}"
	sudo umount "${rootfs}" >/dev/null 2>&1 || true
	sudo mount "${image}" "${rootfs}"

	echo ">>> Install guestd..."
	sudo install -Dm0755 "${REPO_ROOT}/target/release/guestd" "${rootfs}/usr/local/bin/guestd"

	echo ">>> Bootstrap Ubuntu 24.04 (Noble)..."
	sudo debootstrap \
		--arch=amd64 \
		--variant=minbase \
		--components=main,universe \
		--include="${include}" \
		noble \
		"${rootfs}" \
		"${mirror}" >/dev/null 2>&1

	echo ">>> Configure rootfs..."

	# Bind-mount before chrooting
	sudo mount --bind /proc "${rootfs}/proc"
	sudo mount --bind /sys "${rootfs}/sys"
	sudo mount --bind /dev "${rootfs}/dev"
	sudo mount --bind /dev/pts "${rootfs}/dev/pts"

	sudo chroot "${rootfs}" /bin/bash << 'EOF'
# Set hostname
echo "tikovm" > /etc/hostname

# Set up /etc/hosts
cat > /etc/hosts << 'HOSTS'
127.0.0.1   localhost
127.0.1.1   tikovm
HOSTS

# Set root password
echo "root:root" | chpasswd

# Enable serial console for Firecracker (ttyS0)
systemctl enable serial-getty@ttyS0.service

# Set up sshd to allow root login
sed -i 's/#PermitRootLogin prohibit-password/PermitRootLogin yes/' /etc/ssh/sshd_config
systemctl enable ssh

# guestd: vsock guest agent hostd uses to run workloads inside the guest
cat > /etc/systemd/system/guestd.service << 'UNIT'
[Unit]
Description=tikovm guest agent (vsock workload executor)

[Service]
ExecStart=/usr/local/bin/guestd
Restart=on-failure
RestartSec=1

[Install]
WantedBy=multi-user.target
UNIT
systemctl enable guestd

# Networking: eth0's address comes from the kernel `ip=` boot argument
# hostd passes (CONFIG_IP_PNP configures eth0 before init runs), so keep
# this file free of any hardcoded address; only DNS lives here.
mkdir -p /etc/systemd/network
cat > /etc/systemd/network/20-eth0.network << 'NETWORK'
[Match]
Name=eth0

[Network]
DNS=1.1.1.1
NETWORK
systemctl enable systemd-networkd

# minbase doesn't include systemd-resolved, so DNS= above isn't consumed by
# anything - point resolv.conf at the same DNS server directly.
cat > /etc/resolv.conf << 'RESOLV'
nameserver 1.1.1.1
RESOLV

# Set up fstab. The root is an overlayfs assembled by the initramfs
# (lowerdir=/dev/vda = this RO base, upperdir=/dev/vdb = per-VM RW overlay),
# so do NOT list /dev/vda as the root here — that would make systemd try to
# remount it over the overlay.
cat > /etc/fstab << 'FSTAB'
proc      /proc proc  defaults                0 0
sysfs     /sys  sysfs defaults                0 0
tmpfs     /tmp  tmpfs defaults,nosuid,nodev   0 0
FSTAB

# Configure apt sources
cat > /etc/apt/sources.list << 'SOURCES'
deb http://archive.ubuntu.com/ubuntu noble main restricted universe multiverse
deb http://archive.ubuntu.com/ubuntu noble-updates main restricted universe multiverse
deb http://security.ubuntu.com/ubuntu noble-security main restricted universe multiverse
SOURCES

# Set timezone
echo "UTC" > /etc/timezone
ln -sf /usr/share/zoneinfo/UTC /etc/localtime

# Remove artifact of usr-merge
find / -maxdepth 1 -name "*.usr-is-merged" -type d -delete
EOF

	# Unmount in reverse order after chroot exits
	sudo umount "${rootfs}/dev/pts"
	sudo umount "${rootfs}/dev"
	sudo umount "${rootfs}/sys"
	sudo umount "${rootfs}/proc"

	# Per-image extra setup (optional): an entry script may define an
	# `extra_setup <rootfs>` function, invoked here on the host with the
	# image still mounted — e.g. to drop in software not packaged for noble.
	if declare -F extra_setup >/dev/null; then
		echo ">>> Run image-specific extra setup..."
		extra_setup "${rootfs}"
	fi

	echo ">>> Verifying image..."
	if (($#)); then
		sudo chroot "${rootfs}" "$@"
	fi
	sudo umount "${rootfs}"
	# -y: auto-answer yes so non-interactive builds don't abort on minor dirt.
	e2fsck -fy "${image}"

	echo ">>> Done"
}

# setup_s3files <rootfs>
#
# Image-specific setup shared by every rootfs that wants AWS S3 Files support:
# installs amazon-efs-utils + botocore, bakes in the build-time credentials and
# region, and installs a systemd mount unit that auto-mounts the file system at
# /mnt/s3files at boot. Intended to be called from an entry script's
# extra_setup; the config is read from `s3files-config` (git-ignored, copied
# from s3files-config.sample) next to this file.
setup_s3files() {
	local rootfs="$1"
	local config_file="${ROOTFS_SCRIPTS_DIR}/s3files-config"

	if [[ ! -f "${config_file}" ]]; then
		echo "ERROR: ${config_file} not found." >&2
		echo "Copy ${ROOTFS_SCRIPTS_DIR}/s3files-config.sample to s3files-config and fill in real values." >&2
		exit 1
	fi

	# shellcheck source=/dev/null
	source "${config_file}"

	for var in FILE_SYSTEM_ID MOUNT_TARGET_IP AWS_REGION AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY; do
		if [[ -z "${!var:-}" ]]; then
			echo "ERROR: ${var} is not set in ${config_file}." >&2
			exit 1
		fi
	done

	# Stage the config inside the image (0600 — it carries credentials) plus
	# the chroot script that consumes it; both delete themselves at the end.
	sudo install -m 0600 "${config_file}" "${rootfs}/tmp/s3files-config"
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
# VPC-less networking); tls + iam = TLS tunnel + SigV4-signed mount.
#
# Type MUST be s3files, not efs: efs-utils 3.2 ships a dedicated
# /sbin/mount.s3files helper for S3 file systems. Mount targets of an S3
# file system present a TLS cert for *.<region>.s3files.on.aws; the efs
# helper builds checkHost=<fs-id>.efs.<region>.amazonaws.com, which can never
# match, and the TLS tunnel stalls (the NFS mount through it then times out).
# mount.s3files builds checkHost=<fs-id>.s3files.<region>.on.aws instead —
# this is exactly how the production host mounts the same file system.
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
