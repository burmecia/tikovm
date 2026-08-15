import assert from 'node:assert/strict';
import { afterEach, describe, it } from 'node:test';
import { Tikovm } from '../src/client.js';
import { TikovmApiError } from '../src/errors.js';
import { makeVmInstance } from './fixtures.js';
import { apiError, json, noContent, startMockServer } from './mock-server.js';

const servers: Awaited<ReturnType<typeof startMockServer>>[] = [];

afterEach(async () => {
  await Promise.all(servers.splice(0).map((s) => s.close()));
});

async function mock(handler: Parameters<typeof startMockServer>[0]) {
  const server = await startMockServer(handler);
  servers.push(server);
  return server;
}

async function clientFor(baseUrl: string): Promise<Tikovm> {
  return new Tikovm({ accessToken: 'x', baseUrl });
}

describe('vms.list()', () => {
  it('returns a Vm wrapper per instance', async () => {
    const { baseUrl } = await mock((req, res) => {
      assert.equal(req.method, 'GET');
      assert.equal(req.url, '/api/vms');
      json(res, 200, [makeVmInstance('vm-1'), makeVmInstance('vm-2', 'paused')]);
    });
    const vms = await (await clientFor(baseUrl)).vms.list();
    assert.equal(vms.length, 2);
    assert.equal(vms[0]!.id, 'vm-1');
    assert.equal(vms[1]!.id, 'vm-2');
    assert.equal(vms[0]!.state, 'started');
    assert.equal(vms[1]!.state, 'paused');
  });
});

describe('vms.get()', () => {
  it('fetches a single VM and populates its cache', async () => {
    const { baseUrl } = await mock((req, res) => {
      assert.equal(req.url, '/api/vms/vm-1');
      json(res, 200, makeVmInstance('vm-1'));
    });
    const vm = await (await clientFor(baseUrl)).vms.get('vm-1');
    assert.equal(vm.id, 'vm-1');
    assert.equal(vm.state, 'started');
    assert.equal(vm.net?.guest_ip, '172.16.0.2');
  });

  it('throws TikovmApiError on an unknown VM', async () => {
    const { baseUrl } = await mock((_req, res) => apiError(res, 404, 'vm vm-404 not found'));
    await assert.rejects(
      async () => (await clientFor(baseUrl)).vms.get('vm-404'),
      (err) => {
        assert.ok(err instanceof TikovmApiError);
        assert.equal(err.status, 404);
        assert.equal(err.message, 'vm vm-404 not found');
        return true;
      },
    );
  });
});

describe('vms.create()', () => {
  it('POSTs a config with hostd-required defaults filled in', async () => {
    const { baseUrl, requests } = await mock((req, res) => {
      json(res, 201, {
        status: 'created',
        payload: req.body,
        id: 'vm-1',
      });
    });
    const vm = await (await clientFor(baseUrl)).vms.create({
      name: 'my vm',
      project_id: 123,
      image: 'ubuntu-24',
    });

    const body = requests[0]!.body as Record<string, unknown>;
    assert.equal(requests[0]!.url, '/api/vms');
    assert.equal(body.name, 'my vm');
    assert.equal(body.project_id, 123);
    assert.equal(body.image, 'ubuntu-24');
    // mandatory non-defaulted hostd fields get sensible defaults
    assert.equal(body.mode, 'ephemeral');
    assert.equal(body.cpus, 1);
    assert.equal(body.memory_mb, 512);
    assert.equal(body.disk_size_mb, 1024);
    assert.equal(body.ssh_access, false);
    assert.deepEqual(body.network_config, {
      allow_internet: false,
      exposed_ports: [],
      egress: [],
      public_access: false,
    });
    assert.deepEqual(body.env, []);
    assert.equal(body.cron_schedule, null);
    assert.equal(body.auto_suspend, null);
    assert.equal(body.block_storage, null);

    assert.equal(vm.id, 'vm-1');
    assert.equal(vm.vmConfig?.name, 'my vm');
  });

  it('honours explicit overrides', async () => {
    const { baseUrl, requests } = await mock((req, res) =>
      json(res, 201, { status: 'created', payload: req.body, id: 'vm-2' }),
    );
    await (await clientFor(baseUrl)).vms.create({
      name: 'db',
      project_id: 7,
      image: 'postgres-16',
      mode: 'permanent',
      cpus: 4,
      memory_mb: 2048,
      disk_size_mb: 8192,
      ssh_access: true,
      network_config: { allow_internet: true, exposed_ports: [{ port: 5432, label: 'pg' }] },
      env: [{ key: 'PGDATA', value: '/var/lib/postgresql' }],
      tags: ['prod'],
      auto_suspend: { idle_timeout_secs: 300, idle_check_cmd: ['/check'], check_interval_secs: 30 },
    });

    const body = requests[0]!.body as Record<string, unknown>;
    assert.equal(body.mode, 'permanent');
    assert.equal(body.cpus, 4);
    assert.equal(body.ssh_access, true);
    assert.deepEqual(body.network_config, {
      allow_internet: true,
      exposed_ports: [{ port: 5432, label: 'pg' }],
      egress: [],
      public_access: false,
    });
    assert.deepEqual(body.auto_suspend, {
      idle_timeout_secs: 300,
      idle_check_cmd: ['/check'],
      check_interval_secs: 30,
    });
  });

  it('sends auto_suspend with only idle_timeout_secs (hostd defaults the rest)', async () => {
    const { baseUrl, requests } = await mock((req, res) =>
      json(res, 201, { status: 'created', payload: req.body, id: 'vm-2b' }),
    );
    await (await clientFor(baseUrl)).vms.create({
      name: 'tiko-db',
      project_id: 7,
      image: 'tiko-postgres',
      mode: 'permanent',
      auto_suspend: { idle_timeout_secs: 300 },
    });

    const body = requests[0]!.body as Record<string, unknown>;
    // The client resolves hostd's defaults: empty idle_check_cmd lets hostd
    // apply the image's idle check for postgres images; 30s is hostd's
    // default check interval.
    assert.deepEqual(body.auto_suspend, {
      idle_timeout_secs: 300,
      idle_check_cmd: [],
      check_interval_secs: 30,
    });
  });

  it('includes schedule-mode fields', async () => {
    const { baseUrl, requests } = await mock((req, res) =>
      json(res, 201, { status: 'created', payload: req.body, id: 'vm-3' }),
    );
    await (await clientFor(baseUrl)).vms.create({
      name: 'cron',
      project_id: 1,
      image: 'python-3.12',
      mode: 'schedule',
      cmd: ['/run.sh'],
      cron_schedule: '*/5 * * * *',
      timeout_secs: 120,
    });
    const body = requests[0]!.body as Record<string, unknown>;
    assert.deepEqual(body.cmd, ['/run.sh']);
    assert.equal(body.cron_schedule, '*/5 * * * *');
    assert.equal(body.timeout_secs, 120);
  });
});

describe('vms.delete()', () => {
  it('DELETEs the VM and resolves on 204', async () => {
    const { baseUrl, requests } = await mock((req, res) => {
      assert.equal(req.method, 'DELETE');
      assert.equal(req.url, '/api/vms/vm-1');
      noContent(res);
    });
    await (await clientFor(baseUrl)).vms.delete('vm-1');
    assert.equal(requests.length, 1);
  });
});
