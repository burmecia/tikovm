import assert from 'node:assert/strict';
import type { ServerResponse } from 'node:http';
import { afterEach, describe, it } from 'node:test';
import { Tikovm } from '../src/client.js';
import { TikovmApiError } from '../src/errors.js';
import { makeVmInstance } from './fixtures.js';
import { apiError, json, startMockServer } from './mock-server.js';
import type { RecordedRequest } from './mock-server.js';
import type { WorkloadData } from '../src/workload.js';

const servers: Awaited<ReturnType<typeof startMockServer>>[] = [];

afterEach(async () => {
  await Promise.all(servers.splice(0).map((s) => s.close()));
});

async function mock(handler: Parameters<typeof startMockServer>[0]) {
  const server = await startMockServer(handler);
  servers.push(server);
  return server;
}

type Handler = (req: RecordedRequest, res: ServerResponse) => void;

// Each test's handler must also serve the initial `GET /api/vms/vm-1` that
// `vms.get()` issues when building a `Vm` wrapper.
function withVm(handler: Handler): Handler {
  return (req, res) => {
    if (req.url === '/api/vms/vm-1') {
      json(res, 200, makeVmInstance('vm-1'));
      return;
    }
    handler(req, res);
  };
}

function makeWorkload(overrides: Partial<WorkloadData> = {}): WorkloadData {
  return {
    workload_id: 'wl-1',
    vm_id: 'vm-1',
    spec: { cmd: ['echo', 'hi'] },
    state: 'running',
    origin: 'api',
    pid: 42,
    exit_code: null,
    signal: null,
    created_at: '2026-08-10T00:00:00Z',
    started_at: '2026-08-10T00:00:01Z',
    finished_at: null,
    ...overrides,
  };
}

const LOGS = [{ ts: '2026-08-10T00:00:02Z', stream: 'stdout', data: 'hi\n' }];

describe('vm.workloads', () => {
  it('start(spec) POSTs the WorkloadSpec and returns a Workload', async () => {
    const { baseUrl, requests } = await mock(
      withVm((req, res) => {
        assert.equal(req.method, 'POST');
        assert.equal(req.url, '/api/vms/vm-1/workloads');
        json(res, 201, makeWorkload({ spec: req.body as WorkloadData['spec'] }));
      }),
    );
    const vm = await new Tikovm({ accessToken: 'x', baseUrl }).vms.get('vm-1');
    const wl = await vm.workloads.start({ cmd: ['echo', 'hi'] });
    assert.deepEqual(requests[1]!.body, { cmd: ['echo', 'hi'] });
    assert.equal(wl.workload_id, 'wl-1');
    assert.equal(wl.state, 'running');
    assert.equal(wl.isActive, true);
  });

  it('start(cmd, options) builds the spec from a bare command', async () => {
    const { baseUrl, requests } = await mock(
      withVm((req, res) => json(res, 201, makeWorkload({ spec: req.body as WorkloadData['spec'] }))),
    );
    const vm = await new Tikovm({ accessToken: 'x', baseUrl }).vms.get('vm-1');
    await vm.workloads.start(['sh', '-c', 'exit 3'], { env: [{ key: 'A', value: 'b' }], cwd: '/tmp' });
    assert.deepEqual(requests[1]!.body, {
      cmd: ['sh', '-c', 'exit 3'],
      env: [{ key: 'A', value: 'b' }],
      cwd: '/tmp',
    });
  });

  it('start(cmd) omits env/cwd when not given', async () => {
    const { baseUrl, requests } = await mock(
      withVm((req, res) => json(res, 201, makeWorkload({ spec: req.body as WorkloadData['spec'] }))),
    );
    const vm = await new Tikovm({ accessToken: 'x', baseUrl }).vms.get('vm-1');
    await vm.workloads.start(['ls']);
    assert.deepEqual(requests[1]!.body, { cmd: ['ls'] });
  });

  it('list() GETs all workloads as Workload resources', async () => {
    const { baseUrl } = await mock(
      withVm((_req, res) => json(res, 200, [makeWorkload(), makeWorkload({ workload_id: 'wl-2', state: 'exited', exit_code: 0 })] )),
    );
    const vm = await new Tikovm({ accessToken: 'x', baseUrl }).vms.get('vm-1');
    const list = await vm.workloads.list();
    assert.equal(list.length, 2);
    assert.equal(list[0]!.workload_id, 'wl-1');
    assert.equal(list[1]!.state, 'exited');
    assert.equal(list[1]!.exit_code, 0);
  });

  it('get(workloadId) fetches a single workload', async () => {
    const { baseUrl, requests } = await mock(
      withVm((req, res) => {
        assert.equal(req.url, '/api/vms/vm-1/workloads/wl-1');
        json(res, 200, makeWorkload());
      }),
    );
    const vm = await new Tikovm({ accessToken: 'x', baseUrl }).vms.get('vm-1');
    const wl = await vm.workloads.get('wl-1');
    assert.equal(wl.id, 'wl-1');
    assert.equal(wl.vmId, 'vm-1');
    assert.equal(requests[1]!.url, '/api/vms/vm-1/workloads/wl-1');
  });

  it('stop(workloadId) POSTs the stop route and returns the updated workload', async () => {
    const { baseUrl, requests } = await mock(
      withVm((req, res) => {
        assert.equal(req.method, 'POST');
        assert.equal(req.url, '/api/vms/vm-1/workloads/wl-1/stop');
        json(res, 200, makeWorkload({ state: 'stopped', finished_at: '2026-08-10T00:00:02Z' }));
      }),
    );
    const vm = await new Tikovm({ accessToken: 'x', baseUrl }).vms.get('vm-1');
    const wl = await vm.workloads.stop('wl-1');
    assert.equal(wl.state, 'stopped');
    assert.equal(wl.isActive, false);
    assert.equal(requests[1]!.url, '/api/vms/vm-1/workloads/wl-1/stop');
  });

  it('logs(workloadId) GETs the captured output', async () => {
    const { baseUrl, requests } = await mock(
      withVm((req, res) => {
        assert.equal(req.url, '/api/vms/vm-1/workloads/wl-1/logs');
        json(res, 200, LOGS);
      }),
    );
    const vm = await new Tikovm({ accessToken: 'x', baseUrl }).vms.get('vm-1');
    assert.deepEqual(await vm.workloads.logs('wl-1'), LOGS);
    assert.equal(requests[1]!.url, '/api/vms/vm-1/workloads/wl-1/logs');
  });

  it('maps hostd failures (unknown workload) to TikovmApiError', async () => {
    const { baseUrl } = await mock(
      withVm((_req, res) => apiError(res, 404, 'workload wl-nope not found')),
    );
    const vm = await new Tikovm({ accessToken: 'x', baseUrl }).vms.get('vm-1');
    await assert.rejects(
      () => vm.workloads.get('wl-nope'),
      (err) => err instanceof TikovmApiError && err.status === 404 && err.code === 404,
    );
  });
});

describe('Workload resource methods', () => {
  it('refresh()/stop()/logs() update the cache and hit the right routes', async () => {
    const { baseUrl, requests } = await mock((req, res) => {
      if (req.url === '/api/vms/vm-1/workloads/wl-1') {
        json(res, 200, makeWorkload({ state: 'exited', exit_code: 7, finished_at: '2026-08-10T00:00:02Z' }));
      } else if (req.url === '/api/vms/vm-1/workloads/wl-1/stop') {
        json(res, 200, makeWorkload({ state: 'stopped' }));
      } else if (req.url === '/api/vms/vm-1/workloads/wl-1/logs') {
        json(res, 200, LOGS);
      } else {
        json(res, 200, makeVmInstance('vm-1'));
      }
    });
    const client = new Tikovm({ accessToken: 'x', baseUrl });
    const wl = await client.vms.workloads('vm-1').get('wl-1');

    await wl.refresh();
    assert.equal(wl.state, 'exited');
    assert.equal(wl.exit_code, 7);
    assert.equal(requests.length, 2);

    await wl.stop();
    assert.equal(wl.state, 'stopped');
    assert.equal(requests[2]!.url, '/api/vms/vm-1/workloads/wl-1/stop');

    assert.deepEqual(await wl.logs(), LOGS);
    assert.equal(requests[3]!.url, '/api/vms/vm-1/workloads/wl-1/logs');
  });

  it('wait() polls until a terminal state', async () => {
    let calls = 0;
    const { baseUrl } = await mock((req, res) => {
      if (req.url === '/api/vms/vm-1/workloads/wl-1') {
        calls += 1;
        json(res, 200, makeWorkload({ state: calls < 3 ? 'running' : 'exited', exit_code: 0 }));
      } else {
        json(res, 200, makeVmInstance('vm-1'));
      }
    });
    const client = new Tikovm({ accessToken: 'x', baseUrl });
    const wl = await client.vms.workloads('vm-1').get('wl-1');
    await wl.wait({ timeoutMs: 5000, intervalMs: 5 });
    assert.equal(wl.state, 'exited');
    assert.ok(calls >= 3);
  });

  it('wait() times out when the workload stays active', async () => {
    const { baseUrl } = await mock((req, res) => {
      if (req.url === '/api/vms/vm-1/workloads/wl-1') {
        json(res, 200, makeWorkload({ state: 'running' }));
      } else {
        json(res, 200, makeVmInstance('vm-1'));
      }
    });
    const client = new Tikovm({ accessToken: 'x', baseUrl });
    const wl = await client.vms.workloads('vm-1').get('wl-1');
    await assert.rejects(() => wl.wait({ timeoutMs: 50, intervalMs: 5 }), /did not reach a terminal state/);
  });
});
