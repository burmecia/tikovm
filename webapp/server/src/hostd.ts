//! Thin helpers around the `tikovm` Node client (clients/node) that add the
//! polling/retry patterns the demo needs: wait for a VM state, exec until a
//! command succeeds (guest services come up lazily), and 404-tolerant
//! deletes. All hostd traffic goes through here.

import { Tikovm, TikovmApiError } from 'tikovm';
import type { ExecResult, VmState } from 'tikovm';
import type { Config } from './config.js';

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** The slice of hostd's `VmInstance` the overview poll needs. */
export interface RawVmInstance {
  vm_id: string;
  state: VmState;
  created_at: string;
  net: { guest_ip: string } | null;
  vm_config: {
    name: string;
    project_id: number;
    image: string;
    cpus: number;
    memory_mb: number;
    tags: string[];
  };
}

export function makeClient(cfg: Config): Tikovm {
  return new Tikovm({ accessToken: cfg.hostdToken, baseUrl: cfg.hostdUrl });
}

/**
 * Raw `GET /api/vms` for the overview poll (the Vm wrapper does not expose
 * `created_at`, and one plain fetch keeps the 1s poll cheap).
 */
export async function rawListVms(cfg: Config): Promise<RawVmInstance[]> {
  const res = await fetch(`${cfg.hostdUrl}/api/vms`, {
    headers: { Authorization: `Bearer ${cfg.hostdToken}` },
  });
  if (!res.ok) {
    throw new Error(`hostd GET /api/vms failed: ${res.status} ${await res.text()}`);
  }
  return (await res.json()) as RawVmInstance[];
}

/**
 * Raw `GET /api/vms/:id` — used where the Vm wrapper's surface is too
 * narrow (e.g. reading `net.guest_ip` of a suspended VM, which stays
 * host-side state).
 */
export async function rawGetVm(cfg: Config, vmId: string): Promise<RawVmInstance> {
  const res = await fetch(`${cfg.hostdUrl}/api/vms/${vmId}`, {
    headers: { Authorization: `Bearer ${cfg.hostdToken}` },
  });
  if (!res.ok) {
    throw new Error(`hostd GET /api/vms/${vmId} failed: ${res.status} ${await res.text()}`);
  }
  return (await res.json()) as RawVmInstance;
}

/** Poll until the VM reaches `target` (fails on timeout, not on other states). */
export async function waitForState(
  client: Tikovm,
  vmId: string,
  target: VmState,
  timeoutMs: number,
  intervalMs = 1_000,
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const vm = await client.vms.get(vmId).catch((err) => {
      throw new Error(`VM ${vmId} disappeared while waiting for '${target}': ${err}`);
    });
    if (vm.state === target) {
      return;
    }
    if (Date.now() >= deadline) {
      throw new Error(`timed out waiting for VM ${vmId} to reach '${target}' (last: '${vm.state}')`);
    }
    await sleep(intervalMs);
  }
}

/** Run a command in the guest via hostd's synchronous exec wrapper. */
export async function execInVm(
  client: Tikovm,
  vmId: string,
  cmd: string[],
): Promise<ExecResult> {
  const vm = await client.vms.get(vmId);
  return vm.exec(cmd);
}

/** All captured output (stdout + stderr) of an exec, in arrival order. */
export function logsText(r: ExecResult): string {
  return r.logs.map((l) => l.data).join('');
}

/** Exec expecting exit code 0; throws with the captured output otherwise. */
export async function execOk(
  client: Tikovm,
  vmId: string,
  cmd: string[],
  what: string,
): Promise<ExecResult> {
  const r = await execInVm(client, vmId, cmd);
  if (r.exit_code !== 0) {
    throw new Error(`${what} failed (exit ${r.exit_code}):\n${logsText(r)}`);
  }
  return r;
}

/**
 * Exec until it exits 0, retrying on non-zero exits *and* transient hostd
 * errors — used for guest services that come up lazily (the S3 Files mount,
 * postgres accepting connections). hostd's exec waits for guestd (up to 60s)
 * before even running the command, so this also covers slow boots.
 */
export async function execUntil(
  client: Tikovm,
  vmId: string,
  cmd: string[],
  what: string,
  timeoutMs = 240_000,
  intervalMs = 1_000,
): Promise<ExecResult> {
  const deadline = Date.now() + timeoutMs;
  let last: ExecResult | undefined;
  for (;;) {
    try {
      const r = await execInVm(client, vmId, cmd);
      if (r.exit_code === 0) {
        return r;
      }
      last = r;
    } catch {
      // transient hostd/guestd hiccup; keep retrying until the deadline
    }
    if (Date.now() >= deadline) {
      throw new Error(
        `timed out waiting for ${what} (last exit ${last?.exit_code}):\n` +
          `${last ? logsText(last) : 'no successful contact with the guest'}`,
      );
    }
    await sleep(intervalMs);
  }
}

/** Delete a VM, tolerating "already gone" (404). */
export async function deleteVmIfExists(client: Tikovm, vmId: string): Promise<void> {
  try {
    await client.vms.delete(vmId);
  } catch (err) {
    if (err instanceof TikovmApiError && err.status === 404) {
      return;
    }
    throw err;
  }
}

/** Write a file in the guest via base64 (no shell-quoting pitfalls). */
export async function writeGuestFile(
  client: Tikovm,
  vmId: string,
  path: string,
  content: string,
): Promise<void> {
  const b64 = Buffer.from(content, 'utf8').toString('base64');
  const dir = path.slice(0, path.lastIndexOf('/'));
  await execOk(
    client,
    vmId,
    ['bash', '-c', `mkdir -p '${dir}' && printf %s '${b64}' | base64 -d > '${path}'`],
    `write ${path}`,
  );
}

/** Run a login shell as the postgres user (postgres refuses to run as root). */
export function asPostgres(script: string): string[] {
  return ['runuser', '-u', 'postgres', '--', 'bash', '-lc', script];
}

/** Run argv as the postgres user (no shell — safe for arbitrary payloads). */
export function asPostgresArgv(argv: string[]): string[] {
  return ['runuser', '-u', 'postgres', '--', ...argv];
}

export { TikovmApiError };
