//! Builder for the per-project `tiko.env` identity file the tiko-postgres
//! guest image consumes (`/var/lib/postgresql/tiko.env`, sourced by
//! `tiko_env.sh` from `init_pg.sh` / `start_pg.sh`).
//!
//! The image bakes VM-0 identity defaults (org 12 / db 34 / project 56); the
//! demo rewrites the file inside each VM (via the exec API, before
//! `init_pg.sh` runs) so every project gets its own tiko identity. The tiko
//! storage manager namespaces objects as `{TIKO_STORAGE_ROOT}/s3sim/{org}/{db}`,
//! so a globally unique `dbId` per project keeps S3 Files data disjoint.
//!
//! Also here: the seed-pack constants and the `tiko_branch` argv builders
//! used to initialize a project's database by branching it from the seed db
//! (db_id=0) or — for user-requested database branches — from a pack produced
//! by `tiko_branch backup` on another project. Branch packs live next to the
//! seed pack on the shared S3 Files mount (the only path both the source and
//! the branch VM can see — they are in different projects/subnets) and are
//! deleted once the branch passes its sanity check.

/** The identity fields of one demo project's tiko postgres VM. */
export interface TikoIdentity {
  orgId: number;
  dbId: number;
  projectId: number;
  /** hostd's vm id, e.g. `vm-1000-a1b2c3`. */
  vmId: string;
}

const STORAGE_ROOT = '/mnt/s3files/tiko_root';
const LOCAL_PATH = '/var/lib/postgresql/tiko_local';
export const TIKO_ENV_PATH = '/var/lib/postgresql/tiko.env';

/** Seed pack every project database branches from (produced out-of-band by
 * `tiko_branch backup` on the db_id=0 seed; already in place on S3 Files). */
export const SEED_PACK_PATH = '/mnt/s3files/tiko_backup/0.tar.zst';
/** The seed database id every project branches from. */
export const SEED_DB_ID = 0;

/**
 * Pack path for a user-requested database branch: `tiko_branch backup` on the
 * source project writes it, the branch project's restore reads it, and it is
 * deleted once the branch is verified. On the shared S3 Files mount so both
 * VMs (different projects, different subnets) can reach it; named by the
 * branch's globally unique db id, so concurrent branches never collide.
 */
export function branchPackPath(dbId: number): string {
  return `/mnt/s3files/tiko_backup/branch-${dbId}.tar.zst`;
}
/**
 * In-guest PGDATA — must match `DB="tt"` in the image's
 * `tiko_env.sh`/`start_pg.sh`, so the canonical start script and the
 * auto-suspend idle check keep working against the restored cluster.
 */
export const PGDATA = '/var/lib/postgresql/tt';

/** Render the `KEY=value` file contents for the given identity. */
export function buildTikoEnv(identity: TikoIdentity): string {
  return [
    `TIKO_ORG_ID=${identity.orgId}`,
    `TIKO_DB_ID=${identity.dbId}`,
    `TIKO_PROJECT_ID=${identity.projectId}`,
    `TIKO_STORAGE_ROOT=${STORAGE_ROOT}`,
    `TIKO_LOCAL_PATH=${LOCAL_PATH}`,
    `TIKO_VM_ID=${identity.vmId}`,
    '',
  ].join('\n');
}

/** S3 Files namespace this project's tiko objects live under (in-guest path). */
export function tikoNamespace(orgId: number, dbId: number): string {
  return `${STORAGE_ROOT}/s3sim/${orgId}/${dbId}`;
}

/** POSIX single-quote a string for safe interpolation into a shell command. */
export function shellQuote(s: string): string {
  return `'${s.replaceAll("'", `'\\''`)}'`;
}

/**
 * Exec argv (no shell on the outer level — guestd runs argv directly) that
 * writes `tiko.env` as root and hands it to the `postgres` user, the same
 * ownership the base image bakes in. The write lands in the VM's overlay
 * upper layer, so it survives snapshot/restore.
 */
export function tikoEnvWriteCmd(content: string): string[] {
  const lines = content
    .trimEnd()
    .split('\n')
    .map(shellQuote)
    .join(' ');
  return [
    'bash',
    '-c',
    `printf '%s\\n' ${lines} > ${TIKO_ENV_PATH} && ` +
      `chown postgres:postgres ${TIKO_ENV_PATH}`,
  ];
}

/**
 * `tiko_branch backup` argv that packs a running project's database into a
 * tar.zst at `packPath` (pg_basebackup against the local postgres, then
 * pack). Run as the postgres user; the connection flags match how
 * `psqlArgv` reaches the database. The backup is online and consistent, so
 * the source database keeps serving while it runs (and the in-flight exec
 * keeps the source VM's auto-suspend gate from firing mid-backup).
 */
export function branchBackupArgv(packPath: string): string[] {
  return [
    'tiko_branch',
    'backup',
    '--pack',
    packPath,
    '--host',
    '127.0.0.1',
    '--port',
    '5432',
    '--user',
    'postgres',
  ];
}

/**
 * `tiko_branch restore` argv that initializes a fresh project database by
 * branching it from a pack (copy-on-write over the shared
 * `TIKO_STORAGE_ROOT`; the branch namespace `{org}/{dbId}` is seeded from the
 * parent db's base manifest at the backup checkpoint LSN). The pack/parent
 * are the seed constants for a plain project, or a branch pack + the source
 * project's db id for a user-requested branch (nested branching works —
 * ChunkRefs keep their original db id, so a grandchild reads chunks
 * transitively from every ancestor's namespace). Run as the postgres
 * user (restore creates PGDATA 0700 and drives `pg_ctl`, which refuses root);
 * the in-guest `tiko_branch` wrapper sources `tiko.env` for org/storage env,
 * so the per-project `tiko.env` must be written first and the ids passed here
 * must match it. On success the branch is left **stopped** — start it with
 * `start_pg.sh`. `--recovery-timeout 240` keeps the restore inside hostd's
 * 5-minute exec timeout.
 */
export function branchRestoreArgv(restore: {
  packPath: string;
  parentDbId: number;
  dbId: number;
  projectId: number;
}): string[] {
  return [
    'tiko_branch',
    'restore',
    '--pack',
    restore.packPath,
    '--parent-db-id',
    String(restore.parentDbId),
    '--db-id',
    String(restore.dbId),
    '--project-id',
    String(restore.projectId),
    '--pgdata',
    PGDATA,
    '--branch-port',
    '5432',
    '--recovery-timeout',
    '240',
  ];
}
