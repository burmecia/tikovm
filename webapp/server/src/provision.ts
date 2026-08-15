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
//!   4. initialize the database with `tiko_branch restore`, branching from the
//!      seed pack (/mnt/s3files/tiko_backup/0.tar.zst, db_id=0) — every
//!      project db is a copy-on-write branch of the seed
//!   5. start postgres via the image's start_pg.sh (restore leaves the branch
//!      stopped) and verify it answers `select 1`
//!
//! Deleting a project best-effort wipes the project's S3 Files namespace
//! while the guest (which owns the mount) is still alive, then deletes the
//! VMs; hostd tears down the per-project bridge with the last VM.

import type { Tikovm } from 'tikovm';
import type { Config } from './config.js';
import {
  asPostgres,
  asPostgresArgv,
  deleteVmIfExists,
  execInVm,
  execOk,
  execUntil,
  waitForState,
} from './hostd.js';
import type { Project } from './state.js';
import {
  branchRestoreArgv,
  buildTikoEnv,
  tikoEnvWriteCmd,
  tikoNamespace,
} from './tikoenv.js';

export const TIKO_IMAGE = 'tiko-postgres';

/** Images users may add as extra (non-database) VMs to a project. */
export const EXTRA_IMAGES = ['ubuntu-24', 'python-3.12', 'node-22'] as const;
export type ExtraImage = (typeof EXTRA_IMAGES)[number];

/** Provision a project's tiko postgres VM; updates `project` status/step. */
export async function provisionProject(
  cfg: Config,
  client: Tikovm,
  project: Project,
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
      disk_size_mb: 1024,
      network_config: { allow_internet: true },
      // Snapshot the VM after 2 min without client connections (hostd fills
      // in the image's SQL-based idle check); any exec/SQL request wakes it.
      auto_suspend: { idle_timeout_secs: 120 },
      tags: [cfg.demoTag],
    });
    project.vms.push({ vmId: vm.id, name: 'tiko-pg', image: TIKO_IMAGE, kind: 'tiko' });

    setStep('waiting for the VM to boot');
    await waitForState(client, vm.id, 'started', 180_000);

    setStep('waiting for the S3 Files mount');
    await execUntil(
      client,
      vm.id,
      ['mountpoint', '-q', '/mnt/s3files'],
      'the S3 Files mount',
      240_000,
    );

    setStep('writing the per-project tiko.env');
    const env = buildTikoEnv({
      orgId: cfg.orgId,
      dbId: project.dbId,
      projectId: project.id,
      vmId: vm.id,
    });
    await execOk(client, vm.id, tikoEnvWriteCmd(env), 'tiko.env write');

    // Branch the project's database from the seed pack (db_id=0). Runs as the
    // postgres user (restore creates PGDATA 0700 and drives pg_ctl, which
    // refuses root); the in-guest tiko_branch wrapper sources the tiko.env
    // just written, so the ids here must match it.
    setStep('restoring the database from the seed pack');
    await execOk(
      client,
      vm.id,
      asPostgresArgv(
        branchRestoreArgv({ dbId: project.dbId, projectId: project.id }),
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

/** Delete a project and every VM under it; always removes it from the registry. */
export async function deleteProject(
  cfg: Config,
  client: Tikovm,
  remove: (id: number) => void,
  project: Project,
): Promise<void> {
  project.status = 'deleting';
  project.step = 'deleting VMs';
  try {
    const tikoVm = project.vms.find((v) => v.kind === 'tiko');
    if (tikoVm) {
      await cleanupTikoNamespace(cfg, client, tikoVm.vmId, project.dbId);
    }
    await Promise.allSettled(project.vms.map((v) => deleteVmIfExists(client, v.vmId)));
  } finally {
    remove(project.id);
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
