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
# must NOT hardcode a network address: hostd seeds per-VM static network
# config into the overlay upper layer at VM creation time.

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

# Networking: hostd seeds a per-VM static config (Address/Gateway/DNS) into
# the overlay disk's upper layer at VM creation time; it shadows this file
# via overlayfs, so keep this free of any hardcoded address.
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
		echo ">>> Run image-specific setup..."
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
