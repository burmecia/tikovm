//! Project provisioning and teardown — the demo's core workflows.
//!
//! Creating a project boots one tiko-postgres VM and drives it, entirely over
//! the exec API:
//!
//!   1. create the VM (permanent mode, tagged `tikovm-demo`, auto-suspend
//!      after 120s without client connections — any exec/SQL request wakes it)
//!   2. wait for `started` and the S3 Files mount (/mnt/s3files)
//!   3. rewrite /var/lib/postgresql/tiko.env with the project's tiko identity
//!      (persists in the overlay upper layer, survives snapshot/restore)
//!   4. initialize the database with `tiko_branch restore`, branching from a
//!      pack — the seed pack (/mnt/s3files/tiko_backup/0.tar.zst, db_id=0)
//!      for a plain project, or a fresh `tiko_branch backup` of another
//!      project for a user-requested database branch; every project db is a
//!      copy-on-write branch of its parent
//!   5. start postgres via the image's start_pg.sh (restore leaves the branch
//!      stopped) and verify it answers `select 1`
//!
//! Branching a project (`provisionBranch`) prepends step 0: sanity-check the
//! source database (waking its VM if suspended), then `tiko_branch backup`
//! into a pack on the shared S3 Files mount — the only path both VMs can
//! see, since source and branch live in different projects/subnets. The pack
//! is deleted once the branch is verified (or provisioning fails).
//!
//! Deleting a project best-effort wipes the project's S3 Files namespace
//! while the guest (which owns the mount) is still alive, then deletes the
//! VMs; hostd tears down the per-project bridge with the last VM. Deletion
//! **cascades**: a branch reads its ancestors' chunks copy-on-write through
//! the shared storage root, so deleting a project first deletes all its
//! descendant branches (children before parents) — a branch whose ancestor's
//! namespace is wiped is corrupted.

import type { Tikovm } from 'tikovm';
import type { Config } from './config.js';
import {
  asPostgres,
  asPostgresArgv,
  deleteVmIfExists,
  execInVm,
  execOk,
  execUntil,
  rawGetVm,
  waitForState,
} from './hostd.js';
import type { Project, Registry } from './state.js';
import {
  SEED_DB_ID,
  SEED_PACK_PATH,
  branchBackupArgv,
  branchPackPath,
  branchRestoreArgv,
  buildTikoEnv,
  tikoEnvWriteCmd,
  tikoNamespace,
} from './tikoenv.js';

export const TIKO_IMAGE = 'tiko-postgres';

/**
 * Wake the project's tiko postgres VM if it is suspended (its own
 * auto-suspend fires independently of its consumers). Nothing guest-side
 * can wake it — hostd owns that — so DB-facing VMs (lambdas, PostgREST)
 * must go through here before serving a request. The exec restores the VM
 * transparently and the `select 1` both blocks until guestd answers and
 * verifies postgres accepts connections, so callers can connect right away.
 * No-op when the project has no tiko VM or it is already started.
 */
export async function ensureTikoAwake(
  cfg: Config,
  client: Tikovm,
  project: Project,
): Promise<void> {
  const tikoVm = project.vms.find((v) => v.kind === 'tiko');
  if (!tikoVm) {
    return;
  }
  const tiko = await rawGetVm(cfg, tikoVm.vmId);
  if (tiko.state !== 'started') {
    await execOk(client, tikoVm.vmId, psqlArgv('select 1'), 'wake tiko postgres');
  }
}

/** Guest port the project's tiko postgres listens on; exposed at creation so
 * the proxy can forward psql connections (JWT in `tikovm_token`). */
export const TIKO_PG_PORT = 5432;

/** Images users may add as extra (non-database) VMs to a project. The
 * node-22/python-3.12 images are deliberately absent: they are offered as
 * lambda functions instead (see lambda.ts). */
export const EXTRA_IMAGES = ['ubuntu-24'] as const;
export type ExtraImage = (typeof EXTRA_IMAGES)[number];

/** Provision a plain project's tiko postgres VM (branched from the seed
 * pack); updates `project` status/step. */
export async function provisionProject(
  cfg: Config,
  client: Tikovm,
  project: Project,
): Promise<void> {
  await provisionTikoVm(cfg, client, project, {
    packPath: SEED_PACK_PATH,
    parentDbId: SEED_DB_ID,
  });
}

/**
 * Provision a project whose database branches from another project's.
 * Backs up the source database (waking its VM if suspended — the exec API
 * restores it transparently, and the in-flight backup exec blocks the
 * source's auto-suspend gate), then runs the same pipeline as a plain
 * project with the fresh pack and the source's db id as parent. The pack is
 * best-effort deleted afterwards either way; on failure the project is left
 * in `error` with its (half-provisioned) VMs for debugging, same as a plain
 * project.
 */
export async function provisionBranch(
  cfg: Config,
  client: Tikovm,
  project: Project,
  parent: Project,
): Promise<void> {
  const packPath = branchPackPath(project.dbId);
  const parentTikoVm = parent.vms.find((v) => v.kind === 'tiko');
  try {
    if (!parentTikoVm) {
      throw new Error(`source project ${parent.id} has no tiko postgres VM`);
    }
    project.step = 'checking the source database';
    await execOk(client, parentTikoVm.vmId, psqlArgv('select 1'), 'source database readiness');

    project.step = 'backing up the source database';
    await execOk(
      client,
      parentTikoVm.vmId,
      asPostgresArgv(branchBackupArgv(packPath)),
      'tiko_branch backup',
    );

    await provisionTikoVm(cfg, client, project, {
      packPath,
      parentDbId: parent.dbId,
    });
  } catch (err) {
    project.status = 'error';
    project.step = '';
    project.error = err instanceof Error ? err.message : String(err);
  }
  // The pack is a full base backup — large and useless once the branch
  // stands (or never will). Best-effort: prefer the branch VM (just booted,
  // surely running), fall back to the source.
  await removeBranchPack(
    client,
    [project.vms.find((v) => v.kind === 'tiko')?.vmId, parentTikoVm?.vmId],
    packPath,
  );
}

/** Best-effort `rm -f` of a branch pack via the first reachable guest. */
async function removeBranchPack(
  client: Tikovm,
  vmIds: (string | undefined)[],
  packPath: string,
): Promise<void> {
  for (const vmId of vmIds) {
    if (!vmId) {
      continue;
    }
    try {
      await execInVm(client, vmId, ['rm', '-f', packPath]);
      return;
    } catch (err) {
      console.warn(
        `[webapp] branch pack ${packPath} cleanup via ${vmId} failed: ` +
          `${err instanceof Error ? err.message : err}`,
      );
    }
  }
}

/**
 * Shared provisioning pipeline: create the project's tiko postgres VM and
 * branch its database from the given pack; updates `project` status/step.
 */
async function provisionTikoVm(
  cfg: Config,
  client: Tikovm,
  project: Project,
  restore: { packPath: string; parentDbId: number },
): Promise<void> {
  const setStep = (step: string) => {
    project.step = step;
  };
  try {
    setStep('creating the tiko-postgres VM');
    const vm = await client.vms.create({
      name: 'tiko-pg',
      project_id: project.id,
      image: TIKO_IMAGE,
      // permanent (not ephemeral) so hostd can auto-suspend it.
      mode: 'permanent',
      cpus: 1,
      memory_mb: 512,
      disk_size_mb: 2048,
      network_config: {
        allow_internet: true,
        // Expose postgres so the proxy accepts connections to it (the UI's
        // "copy connection string" mints a tcp token for this port).
        exposed_ports: [{ port: TIKO_PG_PORT, label: 'postgres' }],
      },
      // Snapshot the VM after 2 min without client connections (hostd fills
      // in the image's SQL-based idle check); any exec/SQL request wakes it.
      auto_suspend: { idle_timeout_secs: 120 },
      tags: [cfg.demoTag],
    });
    project.vms.push({ vmId: vm.id, name: 'tiko-pg', image: TIKO_IMAGE, kind: 'tiko' });

    setStep('waiting for the VM to boot');
    await waitForState(client, vm.id, 'started', 180_000);

    // The mount wait and the tiko.env write are independent — run them
    // concurrently (the mount comes up during the tail of the guest boot,
    // so this overlaps almost the whole wait with useful work).
    setStep('waiting for the S3 Files mount');
    const env = buildTikoEnv({
      orgId: cfg.orgId,
      dbId: project.dbId,
      projectId: project.id,
      vmId: vm.id,
    });
    const mountReady = execUntil(
      client,
      vm.id,
      ['mountpoint', '-q', '/mnt/s3files'],
      'the S3 Files mount',
      240_000,
    );
    await execOk(client, vm.id, tikoEnvWriteCmd(env), 'tiko.env write');
    await mountReady;

    // Branch the project's database from the pack. Runs as the postgres user
    // (restore creates PGDATA 0700 and drives pg_ctl, which refuses root);
    // the in-guest tiko_branch wrapper sources the tiko.env just written, so
    // the ids here must match it.
    setStep('restoring the database from the pack');
    await execOk(
      client,
      vm.id,
      asPostgresArgv(
        branchRestoreArgv({
          packPath: restore.packPath,
          parentDbId: restore.parentDbId,
          dbId: project.dbId,
          projectId: project.id,
        }),
      ),
      'tiko_branch restore',
    );

    // `tiko_branch restore` deliberately leaves the branch stopped; the
    // image's canonical start script (sources tiko.env, pg_ctl waits for
    // readiness) brings the database up.
    setStep('starting postgres (start_pg.sh)');
    await execOk(
      client,
      vm.id,
      asPostgres('/var/lib/postgresql/start_pg.sh'),
      'postgres start',
    );

    setStep('verifying postgres');
    await execOk(client, vm.id, psqlArgv('select 1'), 'postgres readiness');

    project.status = 'ready';
    project.step = '';
  } catch (err) {
    project.status = 'error';
    project.step = '';
    project.error = err instanceof Error ? err.message : String(err);
    // The (possibly half-provisioned) VM is kept around for debugging; the
    // TTL sweeper / project delete will still clean it up.
  }
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/**
 * Best-effort removal of the project's tiko objects from the S3 Files mount.
 * Only works while a guest that owns the mount is alive; a suspended VM is
 * restored first (the snapshot froze the mount). Strictly bounded: against an
 * unresponsive guest the exec would otherwise stall for minutes, and it must
 * never delay the VM deletions that follow.
 */
async function cleanupTikoNamespace(
  cfg: Config,
  client: Tikovm,
  vmId: string,
  dbId: number,
  budgetMs = 30_000,
): Promise<void> {
  await Promise.race([
    (async () => {
      try {
        const vm = await client.vms.get(vmId);
        if (vm.state === 'suspended') {
          await client.vms.restore(vmId);
        }
        if (vm.state === 'paused') {
          await client.vms.resume(vmId);
        }
        await execInVm(client, vmId, ['rm', '-rf', tikoNamespace(cfg.orgId, dbId)]);
      } catch (err) {
        console.warn(
          `[webapp] S3 namespace cleanup for db ${dbId} skipped: ${err instanceof Error ? err.message : err}`,
        );
      }
    })(),
    sleep(budgetMs).then(() =>
      console.warn(`[webapp] S3 namespace cleanup for db ${dbId} timed out after ${budgetMs}ms`),
    ),
  ]);
}

/**
 * Delete a project and every VM under it; always removes it from the
 * registry. Cascades: branches read this project's chunks copy-on-write, so
 * all descendant branches are deleted first (children before parents).
 * No-ops if the project is already gone or mid-delete — the TTL sweeper and
 * the shutdown handler iterate snapshots of the registry and would otherwise
 * double-delete projects already cascaded by an ancestor.
 */
export async function deleteProject(
  cfg: Config,
  client: Tikovm,
  registry: Registry,
  project: Project,
): Promise<void> {
  const live = registry.get(project.id);
  if (!live || live.status === 'deleting') {
    return;
  }
  for (const child of registry.descendants(project.id)) {
    await deleteProject(cfg, client, registry, child);
  }
  project.status = 'deleting';
  project.step = 'deleting VMs';
  try {
    const tikoVm = project.vms.find((v) => v.kind === 'tiko');
    if (tikoVm) {
      await cleanupTikoNamespace(cfg, client, tikoVm.vmId, project.dbId);
    }
    await Promise.allSettled(project.vms.map((v) => deleteVmIfExists(client, v.vmId)));
  } finally {
    registry.remove(project.id);
  }
}

export interface CreateExtraVmRequest {
  name: string;
  image: string;
  cpus?: number;
  memory_mb?: number;
  disk_size_mb?: number;
}

/** Create an extra (non-tiko) VM under an existing project. */
export async function createExtraVm(
  cfg: Config,
  client: Tikovm,
  project: Project,
  req: CreateExtraVmRequest,
): Promise<string> {
  const vm = await client.vms.create({
    name: req.name,
    project_id: project.id,
    image: req.image,
    mode: 'ephemeral',
    cpus: req.cpus ?? 1,
    memory_mb: req.memory_mb ?? 256,
    disk_size_mb: req.disk_size_mb ?? 1024,
    network_config: { allow_internet: true },
    tags: [cfg.demoTag],
  });
  project.vms.push({ vmId: vm.id, name: req.name, image: req.image, kind: 'extra' });
  return vm.id;
}

/**
 * Exec argv for the SQL panel: psql as the postgres user, no shell on the
 * SQL payload (it rides as a single argv element), ON_ERROR_STOP so errors
 * surface with a non-zero exit. Exec auto-wakes a suspended (auto-suspended)
 * VM, so SQL works even while the VM is parked.
 */
export function psqlArgv(sql: string): string[] {
  return asPostgresArgv([
    '/usr/local/bin/psql',
    '-h',
    '127.0.0.1',
    '-U',
    'postgres',
    '-d',
    'postgres',
    '-v',
    'ON_ERROR_STOP=1',
    '-w',
    '-c',
    sql,
  ]);
}

/**
 * psql command a user runs on their own machine to reach the project's
 * database through hostd's TCP proxy: the minted JWT rides in the
 * `tikovm_token` startup parameter (the proxy verifies and strips it, then
 * splices bytes). The guest pg_hba trusts the project subnet — the host's
 * bridge IP is in it — so no password is needed.
 */
export function psqlConnectionString(cfg: Config, token: string): string {
  return (
    `psql "host=${cfg.proxyHost} port=${cfg.proxyPort} user=postgres ` +
    `dbname=postgres options='-c tikovm_token=${token}'"`
  );
}
