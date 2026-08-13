#!/bin/bash
#
# Build assets/tiko-postgres-rootfs.ext4: the Ubuntu 24.04 s3files base
# (see build_rootfs_s3files.sh / the setup_s3files helper in common.sh) plus
# the customized Tiko PostgreSQL built from the separate tiko repo (default
# /home/ubuntu/tiko — override with TIKO_ROOT). All the generic image work
# lives in common.sh.
#
# The Tiko PostgreSQL is a vendored postgres source tree patched with the
# tiko storage manager (`smgr` crate, statically linked into the postgres
# binary) and the `worker` crate, a background task loaded via
# shared_preload_libraries=libtikoworker that streams WAL. Both are baked into
# this image along with the runtime scripts that init/start the server
# (init_pg.sh / start_pg.sh / tiko_env.sh / postgresql.tiko.conf). Data lives
# on the S3 Files mount at /mnt/s3files (TIKO_STORAGE_ROOT), which is mounted
# at boot by the shared s3files setup; a oneshot service chowns the mount root
# to the postgres user after it comes up.
#
# Prerequisites (not run here — this script only consumes the build outputs):
#   - tiko's build_postgres.sh has run, producing $TIKO_ROOT/target/pg-install
#   - $TIKO_ROOT/target/release/libtikoworker.so has been built (the worker
#     crate is a cdylib; only the debug build is produced by build_postgres.sh)
#   - scripts/rootfs/s3files-config exists (copy s3files-config.sample)
#
# Per-VM identity (TIKO_ORG_ID/TIKO_DB_ID/TIKO_PROJECT_ID) is baked as VM-0
# defaults in /var/lib/postgresql/tiko.env; tiko's host-side VM launcher
# rewrites that file in the overlay upper layer per VM, exactly as the base
# image's defaults are meant to be overridden.
#
# Uses the https apt mirror because this host's egress blocks plain http/80.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=scripts/rootfs/common.sh
source "${SCRIPT_DIR}/common.sh"

TIKO_ROOT="${TIKO_ROOT:-/home/ubuntu/tiko}"
PG_INSTALL="${TIKO_ROOT}/target/pg-install"
WORKER_LIB="${TIKO_ROOT}/target/release/libtikoworker.so"

# Fail fast with actionable messages instead of a confusing mid-build error:
# this script consumes build outputs, it does not build tiko itself.
for missing in "${PG_INSTALL}" "${WORKER_LIB}"; do
	if [[ ! -e "${missing}" ]]; then
		echo "ERROR: ${missing} not found." >&2
		if [[ "${missing}" = "${PG_INSTALL}" ]]; then
			echo "Run ${TIKO_ROOT}/scripts/build_postgres.sh first (builds into target/pg-install)." >&2
		else
			echo "Run 'cargo build --release -p worker' in ${TIKO_ROOT} first (builds target/release/libtikoworker.so)." >&2
		fi
		exit 1
	fi
done

# Host-side hook invoked by build_rootfs with the image mounted.
extra_setup() {
	local rootfs="$1"
	local pghome="${rootfs}/var/lib/postgresql"
	local pglib="${rootfs}/usr/local/lib/postgresql"

	# S3 Files support shared with the s3files image (same s3files-config):
	# efs-utils + botocore, credentials, and the /mnt/s3files boot mount.
	setup_s3files "${rootfs}"

	# PostgreSQL refuses to run as root; create the unprivileged postgres
	# user. useradd --root edits the image's passwd/shadow directly (no chroot
	# needed) and creates the home dir inside the rootfs. Deliberately no sudo:
	# these images are test-only but still more locked down than tiko's own.
	sudo useradd --root "${rootfs}" \
		--system \
		--create-home \
		--home-dir /var/lib/postgresql \
		--shell /bin/bash \
		--user-group \
		postgres
	# Verify the user landed in the image's passwd (id checks the host's
	# passwd, not the rootfs), and grab its numeric uid/gid: chown must use
	# those, since the name only exists inside the image, not on the host.
	grep -q '^postgres:' "${rootfs}/etc/passwd"
	pg_uid="$(awk -F: '$1 == "postgres" { print $3; exit }' "${rootfs}/etc/passwd")"
	pg_gid="$(awk -F: '$1 == "postgres" { print $4; exit }' "${rootfs}/etc/passwd")"

	# The Tiko PostgreSQL install tree: the patched server, libpq, headers,
	# and the shared extension modules. Files land in /usr/local so the
	# postgres binaries resolve themselves without extra PATH/LD tweaks.
	sudo mkdir -p "${rootfs}/usr/local"
	sudo cp -a "${PG_INSTALL}/." "${rootfs}/usr/local/"

	# libtikoworker: the shared_preload_libraries bgworker. It must land in
	# the server's pkglibdir ($libdir == /usr/local/lib/postgresql) or PG
	# cannot load it at startup.
	sudo mkdir -p "${pglib}"
	sudo install -m 0755 "${WORKER_LIB}" "${pglib}/libtikoworker.so"

	# Runtime scripts: init_pg.sh initializes the data dir, start_pg.sh starts
	# the server, tiko_env.sh provides the environment both source (identity +
	# storage paths from tiko.env), postgresql.tiko.conf is the tuned config
	# init_pg.sh drops into the data dir (shared_preload_libraries etc.).
	sudo mkdir -p "${pghome}"
	for f in tiko_env.sh start_pg.sh init_pg.sh postgresql.tiko.conf; do
		sudo install -m 0755 "${TIKO_ROOT}/scripts/${f}" "${pghome}/${f}"
	done

	# Per-VM identity defaults (VM-0). tiko's host-side launcher overrides
	# this per VM via the overlay upper layer; tiko_env.sh treats the file as
	# the single source of truth for org/db/project + storage paths.
	sudo tee "${pghome}/tiko.env" > /dev/null << 'TIKO_ENV'
TIKO_ORG_ID=12
TIKO_DB_ID=34
TIKO_PROJECT_ID=56
TIKO_STORAGE_ROOT=/mnt/s3files/tiko_root
TIKO_LOCAL_PATH=/var/lib/postgresql/tiko_local
TIKO_ENV
	sudo chown "${pg_uid}:${pg_gid}" "${pghome}/tiko.env"
	sudo chown -R "${pg_uid}:${pg_gid}" "${pghome}"

	# /mnt/s3files is mounted at boot by the s3files setup; its mounted root
	# inode is owned by root, so chown the mounted root after it comes up so
	# postgres can create TIKO_STORAGE_ROOT under it (root has ClientRootAccess
	# and the owner persists in S3 Files metadata across remounts). Skip
	# gracefully if the mount is absent.
	sudo tee "${rootfs}/etc/systemd/system/s3files-postgres-owner.service" > /dev/null << 'UNIT'
[Unit]
Description=Make S3 Files mount (/mnt/s3files) writable by postgres
After=mnt-s3files.mount

[Service]
Type=oneshot
ConditionPathIsMountPoint=/mnt/s3files
ExecStart=/bin/chown postgres:postgres /mnt/s3files
RemainAfterExit=yes

[Install]
WantedBy=multi-user.target
UNIT
	sudo mkdir -p "${rootfs}/etc/systemd/system/multi-user.target.wants"
	sudo ln -sf /etc/systemd/system/s3files-postgres-owner.service \
		"${rootfs}/etc/systemd/system/multi-user.target.wants/s3files-postgres-owner.service"
}

# s3files deps plus the runtime shared libraries the Tiko binaries link
# (libz for the server, libreadline8t64+libtinfo6 for psql).
build_rootfs "tiko-postgres-rootfs.ext4" \
	"python3,python3-pip,nfs-common,stunnel4,zlib1g,libreadline8t64" \
	"https://archive.ubuntu.com/ubuntu" \
	bash -c 'test -x /sbin/mount.s3files && /usr/local/bin/postgres --version && test -f /usr/local/lib/postgresql/libtikoworker.so'
