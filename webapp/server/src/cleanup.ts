//! Lifecycle cleanup: the three paths that guarantee demo resources never
//! outlive the webapp.
//!
//! - TTL sweeper: a periodic tick deletes projects past their expiry.
//! - Orphan sweep: on startup, delete any hostd VM still tagged `tikovm-demo`
//!   (leftovers from a crash / kill -9 of a previous webapp run).
//! - Shutdown handler: SIGINT/SIGTERM deletes every registry project (and
//!   tagged strays) before exiting; hostd tears down per-project bridges
//!   automatically with each project's last VM.
//!
//! All three go through `deleteProject`, which cascades into branch
//! descendants and no-ops on already-removed projects — so iterating a
//! registry snapshot here is safe even when a parent's cascade deletes a
//! branch the snapshot also contains.

import type { Server } from 'node:http';
import type { Tikovm } from 'tikovm';
import type { Config } from './config.js';
import { deleteVmIfExists } from './hostd.js';
import type { Project, Registry } from './state.js';
import { deleteProject } from './provision.js';

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** Start the periodic TTL sweeper; returns the timer (unref'd, testable). */
export function startTtlSweeper(
  registry: Registry,
  cfg: Config,
  client: Tikovm,
  intervalMs = 30_000,
): NodeJS.Timeout {
  const timer = setInterval(() => {
    void (async () => {
      for (const project of registry.expired(Date.now())) {
        console.log(`[webapp] project ${project.id} expired — deleting`);
        try {
          await deleteProject(cfg, client, registry, project);
        } catch (err) {
          console.error(`[webapp] TTL delete of project ${project.id} failed:`, err);
        }
      }
    })();
  }, intervalMs);
  timer.unref();
  return timer;
}

/**
 * Delete hostd VMs tagged with the demo tag that the registry does not know
 * about. At startup the registry is empty, so this removes everything a
 * previous webapp run left behind. Returns how many VMs were deleted.
 */
export async function sweepOrphans(
  cfg: Config,
  client: Tikovm,
  registry: Registry,
): Promise<number> {
  const known = new Set(registry.allVmIds());
  let vms;
  try {
    vms = await client.vms.list();
  } catch (err) {
    console.warn('[webapp] orphan sweep: listing VMs failed:', err);
    return 0;
  }
  const orphans = vms.filter(
    (vm) => vm.vmConfig?.tags?.includes(cfg.demoTag) && !known.has(vm.id),
  );
  for (const vm of orphans) {
    console.log(`[webapp] orphan sweep: deleting leftover VM ${vm.id}`);
    await deleteVmIfExists(client, vm.id).catch((err) => {
      console.warn(`[webapp] orphan delete of ${vm.id} failed:`, err);
    });
  }
  return orphans.length;
}

/** Install SIGINT/SIGTERM handlers that delete all demo resources, then exit. */
export function installShutdownHandler(
  cfg: Config,
  client: Tikovm,
  registry: Registry,
  server: Server,
  timeoutMs = 90_000,
): void {
  let shuttingDown = false;
  const shutdown = async (signal: string) => {
    if (shuttingDown) {
      return;
    }
    shuttingDown = true;
    console.log(`[webapp] ${signal} received — cleaning up all demo resources...`);
    server.close();
    const work = (async () => {
      try {
        await Promise.allSettled(
          registry
            .list()
            .map((p: Project) =>
              deleteProject(cfg, client, registry, p),
            ),
        );
        await sweepOrphans(cfg, client, registry);
      } catch (err) {
        // Never let cleanup errors crash the exit path.
        console.warn('[webapp] shutdown cleanup hit an error (exiting anyway):', err);
      }
    })();
    // Never hang the shutdown longer than the deadline, even mid-delete.
    await Promise.race([work, sleep(timeoutMs)]);
    console.log('[webapp] cleanup finished, exiting');
    process.exit(0);
  };
  process.on('SIGINT', () => void shutdown('SIGINT'));
  process.on('SIGTERM', () => void shutdown('SIGTERM'));
}
