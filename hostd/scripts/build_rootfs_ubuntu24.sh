#!/bin/bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ASSETS_DIR="$SCRIPT_DIR/../assets"
IMAGE="$ASSETS_DIR/ubuntu-24.04-rootfs.ext4"
ROOTFS=/tmp/rootfs

echo ">>> Install debootstrap..."
sudo apt update -qq
sudo apt install debootstrap -y >/dev/null 2>&1

echo ">>> Create and mount the image..."
ROOTFS_SIZE_MB="${ROOTFS_SIZE_MB:-4096}"
rm -f "$IMAGE"
truncate -s "${ROOTFS_SIZE_MB}M" "$IMAGE"
mkfs.ext4 "$IMAGE"
mkdir -p "$ROOTFS"
sudo umount "$ROOTFS" >/dev/null 2>&1 || true
sudo mount "$IMAGE" "$ROOTFS"

echo ">>> Bootstrap Ubuntu 24.04 (Noble)..."
sudo debootstrap \
    --arch=amd64 \
    --variant=minbase \
    --components=main,universe \
    --include=systemd,systemd-sysv,udev,sudo,iproute2,iputils-ping,curl,vim,openssh-server,ca-certificates,wget \
    noble \
    "$ROOTFS" \
    http://archive.ubuntu.com/ubuntu >/dev/null 2>&1

echo ">>> Configure rootfs..."

# Bind-mount before chrooting
sudo mount --bind /proc "$ROOTFS/proc"
sudo mount --bind /sys "$ROOTFS/sys"
sudo mount --bind /dev "$ROOTFS/dev"
sudo mount --bind /dev/pts "$ROOTFS/dev/pts"

sudo chroot "$ROOTFS" /bin/bash << 'EOF'
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

# Configure static networking for the Firecracker tap interface (see start_vm.sh)
mkdir -p /etc/systemd/network
cat > /etc/systemd/network/20-eth0.network << 'NETWORK'
[Match]
Name=eth0

[Network]
Address=172.16.0.2/24
Gateway=172.16.0.1
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
sudo umount "$ROOTFS/dev/pts"
sudo umount "$ROOTFS/dev"
sudo umount "$ROOTFS/sys"
sudo umount "$ROOTFS/proc"

echo ">>> Verifying image..."
sudo umount "$ROOTFS"
# -y: auto-answer yes so non-interactive builds don't abort on minor dirt.
e2fsck -fy "$IMAGE"

echo ">>> Done"