import assert from 'node:assert/strict';
import { afterEach, describe, it } from 'node:test';
import { Tikovm } from '../src/client.js';
import { makeVmInstance } from './fixtures.js';
import { json, startMockServer } from './mock-server.js';

const servers: Awaited<ReturnType<typeof startMockServer>>[] = [];

afterEach(async () => {
  await Promise.all(servers.splice(0).map((s) => s.close()));
});

async function mock(handler: Parameters<typeof startMockServer>[0]) {
  const server = await startMockServer(handler);
  servers.push(server);
  return server;
}

async function makeVm(id = 'vm-1') {
  const { baseUrl, requests } = await mock((req, res) => {
    // lifecycle POSTs return the updated VmInstance
    if (req.url === `/api/vms/${id}/pause`) {
      json(res, 200, makeVmInstance(id, 'paused'));
    } else if (req.url === `/api/vms/${id}/resume`) {
      json(res, 200, makeVmInstance(id, 'started'));
    } else if (req.url === `/api/vms/${id}/restore`) {
      json(res, 200, makeVmInstance(id, 'started'));
    } else if (req.url === `/api/vms/${id}/snapshot`) {
      json(res, 200, {
        state_path: `/tmp/tikovm/${id}/${id}_snapshot.state`,
        mem_path: `/tmp/tikovm/${id}/${id}_snapshot.mem`,
        created_at: '2026-08-10T00:00:01Z',
      });
    } else if (req.url === `/api/vms/${id}` && req.method === 'DELETE') {
      res.writeHead(204);
      res.end();
    } else if (req.url === `/api/vms/${id}`) {
      json(res, 200, makeVmInstance(id));
    } else if (req.url === `/api/vms/${id}/exec`) {
      json(res, 200, {
        workload_id: 'wl-1',
        vm_id: id,
        spec: req.body,
        state: 'exited',
        origin: 'api',
        pid: 42,
        exit_code: 0,
        signal: null,
        created_at: '2026-08-10T00:00:02Z',
        started_at: '2026-08-10T00:00:02Z',
        finished_at: '2026-08-10T00:00:03Z',
        logs: [{ ts: '2026-08-10T00:00:03Z', stream: 'stdout', data: 'hello\n' }],
      });
    } else {
      json(res, 404, { error: { code: 404, message: 'unexpected route' } });
    }
  });
  const client = new Tikovm({ accessToken: 'x', baseUrl });
  const vm = await client.vms.get(id);
  return { vm, requests };
}

describe('Vm lifecycle methods', () => {
  it('pause() POSTs and updates the cached state', async () => {
    const { vm, requests } = await makeVm();
    assert.equal(vm.state, 'started');
    await vm.pause();
    assert.equal(requests[1]!.method, 'POST');
    assert.equal(requests[1]!.url, '/api/vms/vm-1/pause');
    assert.equal(vm.state, 'paused');
    assert.equal(vm.isPaused, true);
    assert.equal(vm.isRunning, false);
  });

  it('resume() POSTs and updates the cached state', async () => {
    const { vm } = await makeVm();
    await vm.pause();
    await vm.resume();
    assert.equal(vm.state, 'started');
    assert.equal(vm.isRunning, true);
  });

  it('snapshot() returns the snapshot paths', async () => {
    const { vm, requests } = await makeVm();
    const snapshot = await vm.snapshot();
    assert.equal(requests[1]!.url, '/api/vms/vm-1/snapshot');
    assert.equal(snapshot.state_path.endsWith('_snapshot.state'), true);
    assert.equal(snapshot.mem_path.endsWith('_snapshot.mem'), true);
  });

  it('restore() POSTs and updates the cached state', async () => {
    const { vm } = await makeVm();
    await vm.restore();
    assert.equal(vm.state, 'started');
  });

  it('refresh() re-fetches the current state', async () => {
    const { vm, requests } = await makeVm();
    await vm.refresh();
    assert.equal(requests[1]!.method, 'GET');
    assert.equal(requests[1]!.url, '/api/vms/vm-1');
    assert.equal(vm.state, 'started');
  });

  it('delete() DELETEs and clears the cached state', async () => {
    const { vm, requests } = await makeVm();
    await vm.delete();
    assert.equal(requests[1]!.method, 'DELETE');
    assert.equal(vm.state, undefined);
    assert.equal(vm.isDestroyed, false);
    assert.equal(vm.isRunning, false);
  });

  it('exec() POSTs the workload spec and returns the result plus logs', async () => {
    const { vm, requests } = await makeVm();
    const result = await vm.exec(['echo', 'hello'], { cwd: '/tmp' });
    assert.equal(requests[1]!.method, 'POST');
    assert.equal(requests[1]!.url, '/api/vms/vm-1/exec');
    assert.deepEqual(requests[1]!.body, { cmd: ['echo', 'hello'], env: [], cwd: '/tmp' });
    assert.equal(result.workload_id, 'wl-1');
    assert.equal(result.state, 'exited');
    assert.equal(result.exit_code, 0);
    assert.deepEqual(result.logs, [{ ts: '2026-08-10T00:00:03Z', stream: 'stdout', data: 'hello\n' }]);
  });

  it('exec() omits cwd when not given', async () => {
    const { vm, requests } = await makeVm();
    await vm.exec(['ls']);
    assert.deepEqual(requests[1]!.body, { cmd: ['ls'], env: [] });
  });

  it('vmConfig/net accessors reflect the fetched instance', async () => {
    const { vm } = await makeVm();
    assert.equal(vm.vmConfig?.name, 'vm vm-1');
    assert.equal(vm.vmConfig?.image, 'ubuntu-24');
    assert.equal(vm.net?.guest_ip, '172.16.0.2');
  });
});
