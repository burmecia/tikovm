#!/bin/bash
#
# Build assets/postgres-16-rootfs.ext4: the Ubuntu 24.04 base rootfs plus
# PostgreSQL 16 (noble's stock `postgresql` metapackage). All the real work
# lives in common.sh.
#
# The package's postinst creates the default `16/main` cluster in the image
# and enables postgresql.service, so every VM booting this image gets a
# running PostgreSQL; per-VM data under /var/lib/postgresql lands in the
# VM's overlay upper layer, leaving the shared base untouched.
#
# extra_setup prepares the cluster for per-VM, project-scoped access: it
# listens on all interfaces, and its pg_hba.conf ends with an include_dir
# that hostd drops one subnet-scoped rule into per VM (seeded into the
# overlay upper layer at VM creation time — see seed_overlay_disk in
# hostd/src/vmm/firecracker/setup.rs). Without that seeded rule only
# localhost can connect. The postgres role gets a default password of
# "postgres" (these images are test-only, like the root:root SSH login).
# It also ships /usr/local/bin/tikovm-pg-idle-check, the auto-suspend idle
# check hostd defaults to for postgres-16 VMs (see create_vm in
# hostd/src/vmm/firecracker/vmm.rs).
#
# Uses the https apt mirror because this host's egress blocks plain http/80.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=scripts/rootfs/common.sh
source "${SCRIPT_DIR}/common.sh"

# Host-side hook invoked by build_rootfs with the image mounted.
extra_setup() {
	local rootfs="$1"
	local pgconf="${rootfs}/etc/postgresql/16/main"

	# Listen on every interface: the stock config binds localhost only. Which
	# clients may actually connect is still decided by pg_hba (see below).
	echo "listen_addresses = '*'" | sudo tee "${pgconf}/conf.d/99-tikovm.conf" > /dev/null

	# PostgreSQL 16 supports include directives in pg_hba.conf (paths are
	# bare words there — quoting them is taken literally). hostd seeds one
	# 00-tikovm.conf per VM into this directory, scoped to the VM's project
	# subnet; the empty directory ships in the image so the include_dir
	# never errors.
	sudo tee -a "${pgconf}/pg_hba.conf" > /dev/null << 'HBA'

# tikovm: per-VM rules seeded by hostd into the overlay upper layer.
include_dir /etc/postgresql/16/main/pg_hba.d
HBA

	# The auto-suspend idle check guestd runs when the VM's
	# auto_suspend.idle_check_cmd names it (hostd fills this in by default
	# for postgres-16 VMs with an auto_suspend config).
	sudo tee "${rootfs}/usr/local/bin/tikovm-pg-idle-check" > /dev/null << 'EOF'
#!/bin/bash
# tikovm auto-suspend idle check for PostgreSQL: exit 0 ("idle") when the
# cluster has no client connections and no running activity; anything else
# (connections, active queries, an error) exits non-zero. See
# guestd/src/monitor.rs for how the result is used.
#
# Runs over the local socket with peer auth as the postgres OS user. The
# check's own psql session shows up in pg_stat_activity as a client
# backend, so it must exclude its own pid. A psql failure (e.g. the server
# is still starting) counts as "not idle" — the safe direction.
count="$(su postgres -c "psql -Atqc \"select count(*) from pg_stat_activity where pid <> pg_backend_pid() and (backend_type = 'client backend' or state = 'active')\"")" || exit 1
[ "${count}" = "0" ]
EOF
	sudo chmod 0755 "${rootfs}/usr/local/bin/tikovm-pg-idle-check"

	# Set the default password for the postgres role. No server runs at
	# image build time, so start the cluster socket-only (listen_addresses='')
	# just long enough to ALTER USER, then stop it cleanly — the data
	# directory ships in the image, so the password must be set here.
	sudo tee "${rootfs}/tmp/tikovm-pg-passwd.sh" > /dev/null << 'EOF'
#!/bin/bash
set -euo pipefail
# The socket dir normally comes from systemd tmpfiles at boot; create it
# here since no init system is running in the build chroot.
install -d -o postgres -g postgres /var/run/postgresql
# hostd's seeded pg_hba rules land here (see pg_hba.conf include_dir).
install -d -o postgres -g postgres -m 0755 /etc/postgresql/16/main/pg_hba.d
su postgres -c "pg_ctlcluster 16 main start -o \"-c listen_addresses=''\""
su postgres -c "psql --no-psqlrc -c \"ALTER USER postgres PASSWORD 'postgres';\""
su postgres -c "pg_ctlcluster 16 main stop -m fast"
rm -f /tmp/tikovm-pg-passwd.sh
EOF
	sudo mount --bind /proc "${rootfs}/proc"
	# Same bind/umount pattern as the main chroot in common.sh (a recursive
	# bind of /dev makes /dev/pts busy at umount time on this host). The
	# plain /dev bind still gives PostgreSQL a writable /dev/shm directory.
	sudo mount --bind /dev "${rootfs}/dev"
	sudo mount --bind /dev/pts "${rootfs}/dev/pts"
	# Unmount even when the chroot fails, so a failed build never leaves
	# bind mounts (and a mounted image) behind.
	local status=0
	sudo chroot "${rootfs}" bash /tmp/tikovm-pg-passwd.sh || status=$?
	sudo umount "${rootfs}/dev/pts"
	sudo umount "${rootfs}/dev"
	sudo umount "${rootfs}/proc"
	return "${status}"
}

build_rootfs "postgres-16-rootfs.ext4" "postgresql" "https://archive.ubuntu.com/ubuntu" \
	psql --version
