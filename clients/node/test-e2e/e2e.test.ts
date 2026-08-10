// End-to-end test for the tikovm Node client against a REAL hostd instance.
//
// Run by tests/test_node_client.sh (the bash e2e suite): the wrapper starts
// hostd, compiles this file (see tsconfig.e2e.json) and runs it via
// `node --test` with these env vars set:
//
//   TIKOVM_HOSTD_URL          e.g. http://127.0.0.1:3000 (default matches common.sh)
//   TIKOVM_HOSTD_TOKEN        Bearer token (default 'xxx', common.sh's placeholder)
//   TIKOVM_CREATED_VMS_FILE   path to the bash suite's VM registry, so
//                             cleanup_vms can backstop any leaked VMs
//
// It exercises the whole VM lifecycle through the client library: create
// (with hostd-required defaults filled in), boot, list/get, pause/resume,
// snapshot/restore, exec, error mapping, and delete.

import assert from 'node:assert/strict';
import { appendFileSync, existsSync, readFileSync } from 'node:fs';
import { after, before, describe, it } from 'node:test';
import { setTimeout as sleep } from 'node:timers/promises';

import { Tikovm, TikovmApiError } from '../src/index.js';
import type { Vm } from '../src/index.js';

const baseUrl = process.env.TIKOVM_HOSTD_URL ?? 'http://127.0.0.1:3000';
const token = process.env.TIKOVM_HOSTD_TOKEN ?? 'xxx';
const createdVmsFile = process.env.TIKOVM_CREATED_VMS_FILE;

const PROJECT_ID = 987;
const VM_NAME = 'node-e2e-vm';

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

describe('tikovm Node client against a real hostd', () => {
  let client: Tikovm;
  let vm: Vm;
  const created: string[] = [];

  before(async () => {
    client = new Tikovm({ accessToken: token, baseUrl });
    // Fail fast (before the expensive VM boot) if hostd is unreachable.
    assert.deepEqual(await client.health(), { status: 'ok' });

    // The create endpoint also boots the VM, so the stable state after the
    // boot wait is "started". Assert the echoed config as well: the client
    // fills hostd's mandatory, non-defaulted fields (mode/cpus/memory_mb/
    // disk_size_mb/network_config/ssh_access) when they are omitted.
    vm = await client.vms.create({
      name: VM_NAME,
      project_id: PROJECT_ID,
      image: 'ubuntu-24',
    });
    created.push(vm.id);
    registerVm(vm.id);

    assert.match(vm.id, new RegExp(`^vm-${PROJECT_ID}-`));
    assert.equal(vm.vmConfig?.name, VM_NAME);
    assert.equal(vm.vmConfig?.project_id, PROJECT_ID);
    assert.equal(vm.vmConfig?.mode, 'ephemeral');
    assert.equal(vm.vmConfig?.cpus, 1);
    assert.equal(vm.vmConfig?.memory_mb, 512);
    assert.equal(vm.vmConfig?.disk_size_mb, 1024);
    assert.equal(vm.vmConfig?.ssh_access, false);
    assert.equal(vm.state, undefined); // not fetched yet

    // The client can see the VM once hostd's Firecracker reports Running.
    await waitFor('VM to reach started state', async () => {
      await vm.refresh();
      return vm.state === 'started';
    });
    assert.equal(vm.isRunning, true);

    // guestd (the exec path) is up once systemd has booted to a getty.
    await waitForSerialBoot(vm.id);
  });

  after(async () => {
    // Backstop cleanup; delete() is exercised explicitly in the last test.
    for (const id of created) {
      try {
        await client.vms.delete(id);
      } catch {
        // already deleted
      }
    }
  });

  it('health() reports hostd as ok', async () => {
    assert.deepEqual(await client.health(), { status: 'ok' });
  });

  it('list() and get() find the created VM with live state', async () => {
    const listed = await client.vms.list();
    const found = listed.find((v) => v.id === vm.id);
    assert.ok(found, `expected ${vm.id} in ${listed.map((v) => v.id).join(', ')}`);
    assert.equal(found?.state, 'started');
    assert.equal(found?.vmConfig?.name, VM_NAME);
    assert.match(found?.net?.guest_ip ?? '', /^172\.16\./);

    const got = await client.vms.get(vm.id);
    assert.equal(got.id, vm.id);
    assert.equal(got.state, 'started');
  });

  it('pause() and resume() update the cached state; double pause is a 409', async () => {
    await vm.pause();
    assert.equal(vm.state, 'paused');
    assert.equal(vm.isPaused, true);
    assert.equal(vm.isRunning, false);

    // Pausing a paused VM is rejected by hostd's state machine with a 409.
    await assert.rejects(
      () => vm.pause(),
      (err) => {
        assert.ok(err instanceof TikovmApiError);
        assert.equal(err.status, 409);
        assert.equal(err.code, 409);
        return true;
      },
    );

    await vm.resume();
    assert.equal(vm.state, 'started');
    assert.equal(vm.isRunning, true);

    // The namespace-level convenience methods go through the same path.
    await client.vms.pause(vm.id);
    assert.equal((await client.vms.resume(vm.id)).state, 'started');
    await vm.refresh();
    assert.equal(vm.state, 'started');
  });

  it('snapshot() leaves the VM suspended; restore() brings it back', async () => {
    const snapshot = await vm.snapshot();
    assert.ok(snapshot.state_path.endsWith('_snapshot.state'));
    assert.ok(snapshot.mem_path.endsWith('_snapshot.mem'));
    assert.equal(vm.state, 'suspended');
    assert.equal(vm.isSuspended, true);

    await vm.restore();
    assert.equal(vm.state, 'started');
    assert.equal(vm.isRunning, true);
  });

  it('exec() runs a command in the guest and captures stdout/stderr/exit code', async () => {
    const result = await vm.exec(['sh', '-c', 'echo hello; echo oops >&2; exit 3'], {
      cwd: '/tmp',
    });
    assert.equal(result.state, 'exited');
    assert.equal(result.exit_code, 3);
    assert.ok(result.pid, 'expected a guest pid in the exec result');
    assert.equal(result.spec.cmd[0], 'sh');
    assert.equal(result.spec.cwd, '/tmp');

    const stdout = result.logs.filter((l) => l.stream === 'stdout').map((l) => l.data).join('');
    const stderr = result.logs.filter((l) => l.stream === 'stderr').map((l) => l.data).join('');
    assert.match(stdout, /hello/);
    assert.match(stderr, /oops/);

    const quick = await vm.exec(['echo', 'hi']);
    assert.equal(quick.exit_code, 0);
    assert.ok(quick.logs.some((l) => l.stream === 'stdout' && l.data.includes('hi')));
  });

  it('maps hostd failures to TikovmApiError', async () => {
    await assert.rejects(
      () => client.vms.get('vm-9999-does-not-exist'),
      (err) => {
        assert.ok(err instanceof TikovmApiError);
        assert.equal(err.status, 404);
        assert.equal(err.code, 404);
        assert.ok(err.message.length > 0);
        return true;
      },
    );

    // An unknown image fails at validation (400) before any network or VM is
    // created, so nothing leaks here.
    await assert.rejects(
      () =>
        client.vms.create({
          name: 'bad-image',
          project_id: PROJECT_ID,
          image: 'no-such-image',
        }),
      (err) => {
        assert.ok(err instanceof TikovmApiError);
        assert.equal(err.status, 400);
        return true;
      },
    );
  });

  it('delete() destroys the VM and removes its work dir', async () => {
    await vm.delete();
    assert.equal(vm.state, undefined);

    await assert.rejects(
      () => client.vms.get(vm.id),
      (err) => err instanceof TikovmApiError && err.status === 404,
    );

    await assert.rejects(
      () => client.vms.delete(vm.id),
      (err) => err instanceof TikovmApiError && err.status === 404,
    );

    assert.equal(existsSync(`/tmp/tikovm/${vm.id}`), false, 'work dir should be removed');
  });
});
