import assert from 'node:assert/strict';
import type { ServerResponse } from 'node:http';
import { afterEach, describe, it } from 'node:test';
import { Tikovm } from '../src/client.js';
import { TikovmApiError } from '../src/errors.js';
import { makeVmInstance } from './fixtures.js';
import { apiError, json, noContent, startMockServer } from './mock-server.js';
import type { RecordedRequest } from './mock-server.js';

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

describe('vm.ports', () => {
  it('list() GETs the exposed ports', async () => {
    const { baseUrl, requests } = await mock(
      withVm((req, res) => {
        assert.equal(req.url, '/api/vms/vm-1/ports');
        json(res, 200, [{ port: 8080, label: 'web' }]);
      }),
    );
    const vm = await new Tikovm({ accessToken: 'x', baseUrl }).vms.get('vm-1');
    assert.deepEqual(await vm.ports.list(), [{ port: 8080, label: 'web' }]);
    assert.equal(requests[1]!.method, 'GET');
  });

  it('expose() POSTs the ExposedPort and returns it (201)', async () => {
    const { baseUrl, requests } = await mock(
      withVm((req, res) => {
        assert.equal(req.method, 'POST');
        assert.equal(req.url, '/api/vms/vm-1/ports');
        json(res, 201, req.body);
      }),
    );
    const vm = await new Tikovm({ accessToken: 'x', baseUrl }).vms.get('vm-1');
    const exposed = await vm.ports.expose({ port: 5432, label: 'pg' });
    assert.deepEqual(exposed, { port: 5432, label: 'pg' });
    assert.deepEqual(requests[1]!.body, { port: 5432, label: 'pg' });
  });

  it('remove() DELETEs the port and resolves on 204', async () => {
    const { baseUrl, requests } = await mock(
      withVm((req, res) => {
        assert.equal(req.method, 'DELETE');
        assert.equal(req.url, '/api/vms/vm-1/ports/5432');
        noContent(res);
      }),
    );
    const vm = await new Tikovm({ accessToken: 'x', baseUrl }).vms.get('vm-1');
    await vm.ports.remove(5432);
    assert.equal(requests[1]!.url, '/api/vms/vm-1/ports/5432');
  });

  it('token() mints a JWT with the default proto', async () => {
    const { baseUrl, requests } = await mock(
      withVm((req, res) => {
        assert.equal(req.url, '/api/vms/vm-1/ports/5432/token');
        json(res, 201, { token: 'eyJ.abc', expires_at: '2026-08-10T01:00:00Z' });
      }),
    );
    const vm = await new Tikovm({ accessToken: 'x', baseUrl }).vms.get('vm-1');
    const portToken = await vm.ports.token(5432);
    assert.equal(portToken.token, 'eyJ.abc');
    assert.equal(portToken.expires_at, '2026-08-10T01:00:00Z');
    assert.deepEqual(requests[1]!.body, {}); // ttl_secs/proto omitted
  });

  it('token() forwards ttl_secs and proto when given', async () => {
    const { baseUrl, requests } = await mock(
      withVm((req, res) =>
        json(res, 201, { token: 'eyJ.xyz', expires_at: '2026-08-10T01:00:00Z' }),
      ),
    );
    const vm = await new Tikovm({ accessToken: 'x', baseUrl }).vms.get('vm-1');
    await vm.ports.token(5432, { ttl_secs: 60, proto: 'tcp' });
    assert.deepEqual(requests[1]!.body, { ttl_secs: 60, proto: 'tcp' });
  });

  it('vms.ports(id) is available to id-only callers', async () => {
    const { baseUrl } = await mock((_req, res) => json(res, 200, []));
    const client = new Tikovm({ accessToken: 'x', baseUrl });
    assert.deepEqual(await client.vms.ports('vm-1').list(), []);
  });

  it('maps hostd failures (duplicate port, unexposed token) to TikovmApiError', async () => {
    const { baseUrl } = await mock(
      withVm((req, res) => {
        if (req.url === '/api/vms/vm-1/ports') {
          apiError(res, 409, 'port 8080 is already exposed');
        } else {
          apiError(res, 404, 'port 9999 is not exposed');
        }
      }),
    );
    const vm = await new Tikovm({ accessToken: 'x', baseUrl }).vms.get('vm-1');
    await assert.rejects(
      () => vm.ports.expose({ port: 8080, label: 'web' }),
      (err) => err instanceof TikovmApiError && err.status === 409 && err.code === 409,
    );
    await assert.rejects(
      () => vm.ports.token(9999),
      (err) => err instanceof TikovmApiError && err.status === 404,
    );
  });
});
