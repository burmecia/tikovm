import assert from 'node:assert/strict';
import { afterEach, describe, it } from 'node:test';
import { Tikovm } from '../src/client.js';
import { TikovmApiError } from '../src/errors.js';
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

describe('Tikovm client', () => {
  it('requires a non-empty access token', () => {
    assert.throws(() => new Tikovm({ accessToken: '' }), TypeError);
    assert.throws(() => new Tikovm({ accessToken: '   ' }), TypeError);
    assert.throws(() => new Tikovm({ accessToken: 'x', baseUrl: '' }), TypeError);
  });

  it('defaults baseUrl to http://localhost:3000 and strips trailing slashes', async () => {
    const { baseUrl, requests } = await mock((_req, res) => json(res, 200, { status: 'ok' }));
    const client = new Tikovm({ accessToken: 'secret', baseUrl: `${baseUrl}/` });
    await client.health();
    assert.equal(requests[0]!.url, '/api/health');
  });

  it('sends the bearer token on every request', async () => {
    const { baseUrl, requests } = await mock((_req, res) => json(res, 200, { status: 'ok' }));
    const client = new Tikovm({ accessToken: 's3cr3t', baseUrl });
    await client.health();
    assert.equal(requests[0]!.headers.authorization, 'Bearer s3cr3t');
  });

  it('health() returns the hostd status', async () => {
    const { baseUrl } = await mock((_req, res) => json(res, 200, { status: 'ok' }));
    const client = new Tikovm({ accessToken: 'x', baseUrl });
    assert.deepEqual(await client.health(), { status: 'ok' });
  });

  it('maps non-2xx responses to TikovmApiError with the uniform error body', async () => {
    const { baseUrl } = await mock((_req, res) => apiError(res, 401, 'missing or invalid bearer token'));
    const client = new Tikovm({ accessToken: 'wrong', baseUrl });
    await assert.rejects(
      () => client.health(),
      (err) => {
        assert.ok(err instanceof TikovmApiError);
        assert.equal(err.status, 401);
        assert.equal(err.code, 401);
        assert.equal(err.message, 'missing or invalid bearer token');
        return true;
      },
    );
  });

  it('falls back to a default message when the error body is not JSON', async () => {
    const { baseUrl } = await mock((_req, res) => {
      res.writeHead(500);
      res.end('boom');
    });
    const client = new Tikovm({ accessToken: 'x', baseUrl });
    await assert.rejects(
      () => client.health(),
      (err) => {
        assert.ok(err instanceof TikovmApiError);
        assert.equal(err.status, 500);
        assert.equal(err.message, 'HTTP 500');
        return true;
      },
    );
  });

  it('resolves void responses (204) without a body', async () => {
    const { baseUrl } = await mock((_req, res) => noContent(res));
    const client = new Tikovm({ accessToken: 'x', baseUrl });
    await client.vms.delete('vm-1-test');
  });

  it('surfaces transport failures as TikovmRequestError', async () => {
    const client = new Tikovm({ accessToken: 'x', baseUrl: 'http://127.0.0.1:1' });
    await assert.rejects(() => client.health(), { name: 'TikovmRequestError' });
  });
});
