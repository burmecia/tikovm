//! PostgREST database APIs on top of the `postgrest` image.
//!
//! A postgrest VM is a `permanent` VM running the baked-in PostgREST binary
//! as a systemd service (port 3000), configured with a db-uri pointing at
//! the project's tiko postgres guest IP (same subnet, pg_hba-trusted — no
//! password). Requests arrive through hostd's HTTP proxy:
//!
//!   caller → webapp `ALL /api/demo/pgrst/<slug>/<table…>` → mint 60s proxy
//!   JWT → hostd proxy → guest :3000
//!
//! Same mechanics as lambdas (see lambda.ts): permanent mode + exposed port
//! + `auto_suspend.idle_timeout_secs` gives idle snapshotting for free, the
//! proxy transparently restores a suspended VM on the next request, and the
//! project's tiko VM is woken first (its own auto-suspend is independent).

import type { Tikovm } from 'tikovm';
import type { Config } from './config.js';
import {
  execOk,
  execUntil,
  rawGetVm,
  waitForState,
  writeGuestFile,
} from './hostd.js';
import { ensureTikoAwake } from './provision.js';
import type { Project, ProjectVmEntry } from './state.js';

/** Guest port PostgREST listens on; exposed at VM creation. */
export const PGRST_PORT = 3000;

export const PGRST_IMAGE = 'postgrest';

/** Idle seconds without a request before hostd snapshots the VM. */
export const PGRST_IDLE_TIMEOUT_SECS = 120;

/** Thrown when requesting a postgrest VM that is still deploying / in error. */
export class PostgrestNotReady extends Error {}

/** postgrest.conf pointing at the project's tiko postgres. */
function pgrstConfig(dbHost: string): string {
  // db-anon-role = postgres: anonymous requests run as the superuser
  // (demo-only — the pg_hba trust rule already scopes access to the project
  // subnet). db-schemas limits the API to public.
  return (
    `db-uri = "postgres://postgres@${dbHost}:5432/postgres"\n` +
    `db-schemas = "public"\n` +
    `db-anon-role = "postgres"\n`
  );
}

const PGRST_UNIT = `[Unit]
Description=PostgREST (tikovm database API)
After=network.target

[Service]
ExecStart=/usr/local/bin/postgrest /etc/postgrest.conf
Restart=always
RestartSec=1

[Install]
WantedBy=multi-user.target
`;

/**
 * Provision a postgrest VM asynchronously; updates `entry.postgrest`
 * status/step/error. Same entry/vmId placeholder policy as
 * provisionLambda. On failure the entry is left in `error` with its VM for
 * debugging (or dropped if the VM was never created).
 */
export async function provisionPostgrest(
  cfg: Config,
  client: Tikovm,
  project: Project,
  entry: ProjectVmEntry,
): Promise<void> {
  const meta = entry.postgrest!;
  const setStep = (step: string) => {
    meta.step = step;
  };
  try {
    setStep('creating the VM');
    const vm = await client.vms.create({
      name: entry.name,
      project_id: project.id,
      image: PGRST_IMAGE,
      // permanent (not ephemeral) so hostd can auto-suspend it.
      mode: 'permanent',
      cpus: 1,
      memory_mb: 256,
      disk_size_mb: 1024,
      network_config: {
        allow_internet: true,
        exposed_ports: [{ port: PGRST_PORT, label: 'postgrest' }],
      },
      auto_suspend: { idle_timeout_secs: PGRST_IDLE_TIMEOUT_SECS },
      tags: [cfg.demoTag],
    });
    entry.vmId = vm.id;

    setStep('waiting for the VM to boot');
    await waitForState(client, vm.id, 'started', 180_000);

    // PostgREST loads its schema cache at startup from the database, so the
    // tiko VM must be up first (and its guest IP is host-side state,
    // readable even while suspended).
    setStep('waking the tiko postgres VM');
    await ensureTikoAwake(cfg, client, project);
    const tikoVm = project.vms.find((v) => v.kind === 'tiko');
    if (!tikoVm) {
      throw new Error(`project ${project.id} has no tiko postgres VM`);
    }
    const tiko = await rawGetVm(cfg, tikoVm.vmId);
    const dbHost = tiko.net?.guest_ip;
    if (!dbHost) {
      throw new Error('the tiko postgres VM has no guest IP yet');
    }

    setStep('installing postgrest');
    await writeGuestFile(client, vm.id, '/etc/postgrest.conf', pgrstConfig(dbHost));
    await writeGuestFile(client, vm.id, '/etc/systemd/system/postgrest.service', PGRST_UNIT);
    await execOk(client, vm.id,
      ['systemctl', 'enable', '--now', 'postgrest.service'],
      'postgrest service start');

    setStep('waiting for postgrest');
    await execUntil(
      client,
      vm.id,
      ['curl', '-fsS', `http://127.0.0.1:${PGRST_PORT}/`],
      'postgrest',
      60_000,
    );

    meta.status = 'ready';
    meta.step = '';
    console.log(
      `[webapp] postgrest ${meta.slug} ready on VM ${vm.id} ` +
        `(project ${project.id}, db ${dbHost})`,
    );
  } catch (err) {
    meta.status = 'error';
    meta.step = '';
    meta.error = err instanceof Error ? err.message : String(err);
    console.error(`[webapp] postgrest ${meta.slug} deploy failed: ${meta.error}`);
    if (!entry.vmId) {
      project.vms = project.vms.filter((v) => v !== entry);
    }
  }
}

export interface PgrstResult {
  status: number;
  contentType: string;
  contentRange: string | null;
  body: string;
  durationMs: number;
}

/**
 * PostgREST error codes meaning "the schema cache is stale": the table
 * (PGRST205) or column (PGRST204) was created after PostgREST last loaded
 * its cache. Both are fixed by a cache reload, which PostgREST triggers on
 * SIGUSR1.
 */
const STALE_CACHE_CODES = new Set(['PGRST204', 'PGRST205']);

/** Extract the PostgREST error code from an error response, if it is one. */
function pgrstErrorCode(result: PgrstResult): string | null {
  if (result.status !== 400 && result.status !== 404) {
    return null;
  }
  try {
    const code = (JSON.parse(result.body) as { code?: unknown }).code;
    return typeof code === 'string' ? code : null;
  } catch {
    return null;
  }
}

/**
 * Forward one REST request to the VM's PostgREST: wake the project's tiko
 * VM if suspended (nothing guest-side can), mint a short-lived proxy JWT,
 * and relay method/path/query/body through hostd's HTTP proxy. Upstream
 * status/content-type/body pass through verbatim.
 */
export async function proxyPostgrest(
  cfg: Config,
  client: Tikovm,
  project: Project,
  entry: ProjectVmEntry,
  req: {
    method: string;
    path: string;
    queryString: string;
    body: string;
    contentType: string | undefined;
    headers: Record<string, string | undefined>;
  },
): Promise<PgrstResult> {
  const meta = entry.postgrest!;
  if (meta.status !== 'ready') {
    throw new PostgrestNotReady(
      `postgrest ${meta.slug} is not ready (status: ${meta.status})`,
    );
  }
  await ensureTikoAwake(cfg, client, project);
  const { token } = await client.vms
    .ports(entry.vmId)
    .token(PGRST_PORT, { proto: 'http', ttl_secs: 60 });

  const proxyBase = `http://${new URL(cfg.hostdUrl).hostname}:${cfg.proxyPort}`;
  const started = Date.now();
  const canHaveBody = req.method !== 'GET' && req.method !== 'HEAD';
  // Forward the headers PostgREST features depend on (Prefer:
  // return=representation/count, Accept: media type, Range: pagination).
  const headers: Record<string, string> = { authorization: `Bearer ${token}` };
  for (const h of ['accept', 'prefer', 'range'] as const) {
    const v = req.headers[h];
    if (v) {
      headers[h] = v;
    }
  }
  if (canHaveBody && req.contentType) {
    headers['content-type'] = req.contentType;
  }
  const attempt = async (): Promise<PgrstResult> => {
    const upstream = await fetch(`${proxyBase}${req.path}${req.queryString}`, {
      method: req.method,
      headers,
      body: canHaveBody && req.body ? req.body : undefined,
      signal: AbortSignal.timeout(30_000),
    });
    return {
      status: upstream.status,
      contentType: upstream.headers.get('content-type') ?? 'application/json',
      // Content-Range carries pagination info for Prefer: count requests.
      contentRange: upstream.headers.get('content-range'),
      body: await upstream.text(),
      durationMs: Date.now() - started,
    };
  };

  let result = await attempt();
  let code = pgrstErrorCode(result);
  if (code && STALE_CACHE_CODES.has(code)) {
    // The table/column was created after PostgREST loaded its schema
    // cache: ask it to reload (SIGUSR1) and retry while the stale-cache
    // error persists. The reload itself is sub-second once the DB
    // connection is re-established; allow for a slow reconnect.
    console.log(
      `[webapp] postgrest ${meta.slug}: ${code}, reloading schema cache`,
    );
    await execOk(
      client,
      entry.vmId,
      ['systemctl', 'kill', '-s', 'SIGUSR1', 'postgrest.service'],
      'postgrest schema cache reload',
    );
    const deadline = Date.now() + 20_000;
    while (code && STALE_CACHE_CODES.has(code) && Date.now() < deadline) {
      await new Promise((resolve) => setTimeout(resolve, 2_000));
      result = await attempt();
      code = pgrstErrorCode(result);
    }
  }
  return result;
}
