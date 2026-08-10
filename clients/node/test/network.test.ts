import assert from 'node:assert/strict';
import { afterEach, describe, it } from 'node:test';
import { Tikovm } from '../src/client.js';
import { TikovmApiError } from '../src/errors.js';
import { makeVmInstance } from './fixtures.js';
import { apiError, json, startMockServer } from './mock-server.js';

const servers: Awaited<ReturnType<typeof startMockServer>>[] = [];

afterEach(async () => {
  await Promise.all(servers.splice(0).map((s) => s.close()));
});

async function mock(handler: Parameters<typeof startMockServer>[0]) {
  const server = await startMockServer(handler);
  servers.push(server);
  return server;
}

const NETWORK_CONFIG = {
  allow_internet: true,
  exposed_ports: [
    { port: 5432, label: 'pg' },
    { port: 8080, label: 'web' },
  ],
  egress: [],
  public_access: false,
};

describe('vm.network', () => {
  it('get() fetches the VM\'s live NetworkConfig', async () => {
    const { baseUrl, requests } = await mock((req, res) => {
      if (req.url === '/api/vms/vm-1') {
        json(res, 200, makeVmInstance('vm-1'));
      } else {
        assert.equal(req.url, '/api/vms/vm-1/network');
        json(res, 200, NETWORK_CONFIG);
      }
    });
    const vm = await new Tikovm({ accessToken: 'x', baseUrl }).vms.get('vm-1');
    const network = await vm.network.get();
    assert.deepEqual(network, NETWORK_CONFIG);
    assert.equal(requests.length, 2); // get('vm-1') + network.get()
  });

  it('vms.network(id) is available to id-only callers', async () => {
    const { baseUrl, requests } = await mock((_req, res) => json(res, 200, NETWORK_CONFIG));
    const client = new Tikovm({ accessToken: 'x', baseUrl });
    const network = await client.vms.network('vm-1').get();
    assert.equal(network.allow_internet, true);
    assert.equal(network.exposed_ports[0]!.label, 'pg');
    assert.equal(requests[0]!.url, '/api/vms/vm-1/network');
  });

  it('surfaces hostd failures as TikovmApiError', async () => {
    const { baseUrl } = await mock((req, res) => {
      if (req.url === '/api/vms/vm-1') {
        json(res, 200, makeVmInstance('vm-1'));
      } else {
        apiError(res, 404, 'vm vm-1 not found');
      }
    });
    const vm = await new Tikovm({ accessToken: 'x', baseUrl }).vms.get('vm-1');
    await assert.rejects(
      () => vm.network.get(),
      (err) => err instanceof TikovmApiError && err.status === 404,
    );
  });
});
