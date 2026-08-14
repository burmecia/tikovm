// End-to-end test for the tiko-postgres guest image
// (assets/tiko-postgres-rootfs.ext4): boot a VM from it, initialize and start
// the Tiko PostgreSQL server (the image's init_pg.sh / start_pg.sh, run as
// the postgres user), run a psql smoke test that round-trips data through the
// tiko storage manager (S3 Files-backed), then tear everything down —
// including the objects the test wrote to the S3 Files mount.
//
// Run by tests/test_tiko_postgres.sh (the bash e2e suite): the wrapper starts
// hostd, compiles this file (see tsconfig.e2e.json) and runs it via
// `node --test` with these env vars set:
//
//   TIKOVM_HOSTD_URL          e.g. http://127.0.0.1:3000 (default matches common.sh)
//   TIKOVM_HOSTD_TOKEN        Bearer token (default 'xxx', common.sh's placeholder)
//   TIKOVM_CREATED_VMS_FILE   path to the bash suite's VM registry, so
//                             cleanup_vms can backstop any leaked VMs

import assert from 'node:assert/strict';
import { appendFileSync, existsSync, readFileSync } from 'node:fs';
import { after, before, describe, it } from 'node:test';
import { setTimeout as sleep } from 'node:timers/promises';

import { Tikovm, TikovmApiError } from '../src/index.js';
import type { ExecResult, Vm } from '../src/index.js';

const baseUrl = process.env.TIKOVM_HOSTD_URL ?? 'http://127.0.0.1:3000';
const token = process.env.TIKOVM_HOSTD_TOKEN ?? 'xxx';
const createdVmsFile = process.env.TIKOVM_CREATED_VMS_FILE;

const PROJECT_ID = 456;
const VM_NAME = 'tiko-postgres-e2e';
// The image's baked tiko.env is the VM-0 identity (org 12 / db 34); the tiko
// storage manager writes under {TIKO_STORAGE_ROOT}/s3sim/{org}/{db}, so this
// is exactly the namespace the test VM populates on the S3 Files mount.
const TIKO_NS = '/mnt/s3files/tiko_root/s3sim/12/34';

// Append created VM ids to the bash suite's registry file so its EXIT trap
// (cleanup_vms) can tear down any VMs a crashed test run leaves behind.
function registerVm(id: string): void {
  if (createdVmsFile) {
    try {
      appendFileSync(createdVmsFile, `${id}\n`);
    } catch {
      // best-effort backstop only
    }
  }
}

async function waitFor(
  what: string,
  predicate: () => boolean | Promise<boolean>,
  timeoutMs = 60_000,
  intervalMs = 200,
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await predicate()) {
      return;
    }
    await sleep(intervalMs);
  }
  throw new Error(`timed out waiting for ${what} after ${timeoutMs}ms`);
}

// Mirror of common.sh's wait_serial_boot: the initramfs assembles the
// overlay root and switch_roots into systemd, which starts a getty on ttyS0
// ("login:"). guestd (the exec path) is a systemd service in the same boot.
async function waitForSerialBoot(vmId: string): Promise<void> {
  const log = `/tmp/tikovm/${vmId}/${vmId}.serial.log`;
  await waitFor('VM to reach a login prompt', () => {
    if (!existsSync(log)) {
      return false;
    }
    const text = readFileSync(log, 'utf8');
    if (text.includes('dropping to rescue shell')) {
      throw new Error(`VM boot failed: init dropped to rescue shell (${log})`);
    }
    return text.includes('login:');
  }, 60_000);
}

// Run a command through the exec wrapper as the postgres user. The guestd
// exec path runs as root, so init/start/psql go through runuser to drop
// privileges (postgres refuses to run as root).
function asPostgres(script: string): string[] {
  return ['runuser', '-u', 'postgres', '--', 'bash', '-lc', script];
}

function stdoutOf(r: ExecResult): string {
  return r.logs.filter((l) => l.stream === 'stdout').map((l) => l.data).join('');
}

function allLogs(r: ExecResult): string {
  return r.logs.map((l) => l.data).join('');
}

async function execOk(vm: Vm, cmd: string[], what: string): Promise<ExecResult> {
  const r = await vm.exec(cmd);
  assert.equal(r.exit_code, 0, `${what} failed (exit ${r.exit_code}):\n${allLogs(r)}`);
  return r;
}

async function execUntil(
  vm: Vm,
  cmd: string[],
  what: string,
  timeoutMs = 180_000,
  intervalMs = 2_000,
): Promise<ExecResult> {
  const deadline = Date.now() + timeoutMs;
  let last: ExecResult | undefined;
  while (Date.now() < deadline) {
    try {
      const r = await vm.exec(cmd);
      if (r.exit_code === 0) {
        return r;
      }
      last = r;
    } catch {
      // transient hostd/guestd hiccup while postgres is coming up; retry
    }
    await sleep(intervalMs);
  }
  throw new Error(
    `timed out waiting for ${what} (last exit ${last?.exit_code}):\n${last ? allLogs(last) : ''}`,
  );
}

describe('tiko-postgres guest image', () => {
  let client: Tikovm;
  let vm: Vm;
  const created: string[] = [];

  before(async () => {
    client = new Tikovm({ accessToken: token, baseUrl });
    // Fail fast (before the expensive VM boot) if hostd is unreachable.
    assert.deepEqual(await client.health(), { status: 'ok' });

    vm = await client.vms.create({
      name: VM_NAME,
      project_id: PROJECT_ID,
      image: 'tiko-postgres',
      network_config: { allow_internet: true },
    });
    created.push(vm.id);
    registerVm(vm.id);

    await waitFor('VM to reach started state', async () => {
      await vm.refresh();
      return vm.state === 'started';
    });
    await waitForSerialBoot(vm.id);
  });

  after(async () => {
    // Backstop cleanup; the last test deletes the VM explicitly.
    for (const id of created) {
      try {
        await client.vms.delete(id);
      } catch {
        // already deleted
      }
    }
  });

  it('mounts the S3 Files filesystem at /mnt/s3files', async () => {
    // The mount unit lags the login prompt (TLS tunnel + IAM handshake), and
    // initdb writes through the tiko storage manager straight to S3 Files, so
    // nothing below runs until the mount is up.
    await execUntil(vm, ['mountpoint', '-q', '/mnt/s3files'], 'S3 Files mount to come up');
  });

  it('initializes the database (init_pg.sh)', async () => {
    // Fresh slate on the S3 side: a stale namespace from a previous run would
    // otherwise leak into this one.
    await vm.exec(['rm', '-rf', TIKO_NS]);
    await execOk(vm, asPostgres('/var/lib/postgresql/init_pg.sh'), 'init_pg.sh');
  });

  it('starts the database (start_pg.sh)', async () => {
    await execOk(vm, asPostgres('/var/lib/postgresql/start_pg.sh'), 'start_pg.sh');
    // Wait until the server answers a query.
    await execUntil(
      vm,
      asPostgres('/usr/local/bin/psql -h 127.0.0.1 -U postgres -d postgres -tAc "select 1"'),
      'postgres to accept connections',
    );
  });

  it('runs a psql smoke test against the database', async () => {
    await execOk(
      vm,
      asPostgres(
        '/usr/local/bin/psql -h 127.0.0.1 -U postgres -d postgres ' +
          '-c "create table smoke(id int primary key, note text); ' +
          "insert into smoke values (1, 'tiko'), (2, 'postgres');\"",
      ),
      'create table + insert',
    );
    const count = await execOk(
      vm,
      asPostgres('/usr/local/bin/psql -h 127.0.0.1 -U postgres -d postgres -tAc "select count(*), min(note) from smoke;"'),
      'select count/min',
    );
    assert.match(stdoutOf(count), /2\|postgres/);
    await execOk(
      vm,
      asPostgres('/usr/local/bin/psql -h 127.0.0.1 -U postgres -d postgres -c "drop table smoke;"'),
      'drop table',
    );
  });

  it('the smoke-test data landed on the S3 Files mount', async () => {
    // initdb + the queries above must have written tiko objects (chunks,
    // base manifests, WAL) under the VM's org/db namespace.
    const files = await execOk(
      vm,
      ['sh', '-c', `find ${TIKO_NS} -type f | wc -l`],
      'count S3 Files objects',
    );
    const n = parseInt(stdoutOf(files).trim(), 10);
    assert.ok(Number.isFinite(n) && n > 0, `expected tiko objects under ${TIKO_NS}, found ${n}`);
  });

  it('tears down: stops the server, removes S3 Files data, deletes the VM', async () => {
    // Stop postgres cleanly so the S3 Files cleanup below races nothing.
    await vm.exec(asPostgres('pg_ctl -D /var/lib/postgresql/tt stop -m fast')).catch(() => {
      // server may already be down
    });
    // Remove the tiko objects this test wrote to the S3 Files mount while the
    // guest (which owns the mount) is still alive.
    await vm.exec(['rm', '-rf', TIKO_NS]);

    await vm.delete();
    assert.equal(vm.state, undefined);
    await assert.rejects(
      () => client.vms.get(vm.id),
      (err) => err instanceof TikovmApiError && err.status === 404,
    );
  });
});
