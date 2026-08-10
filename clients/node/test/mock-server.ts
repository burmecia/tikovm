import { createServer } from 'node:http';
import type { ServerResponse } from 'node:http';

export interface RecordedRequest {
  method: string;
  url: string;
  headers: Record<string, string>;
  body: unknown;
}

export interface MockServer {
  baseUrl: string;
  requests: RecordedRequest[];
  close: () => Promise<void>;
}

/** Start a throwaway HTTP server that records every request and delegates responses to `handler`. */
export async function startMockServer(
  handler: (req: RecordedRequest, res: ServerResponse) => void,
): Promise<MockServer> {
  const requests: RecordedRequest[] = [];
  const server = createServer(async (req, res) => {
    const chunks: Buffer[] = [];
    for await (const chunk of req) {
      chunks.push(chunk as Buffer);
    }
    const rawBody = Buffer.concat(chunks).toString('utf8');
    const headers: Record<string, string> = {};
    for (const [key, value] of Object.entries(req.headers)) {
      headers[key] = typeof value === 'string' ? value : (value?.join(', ') ?? '');
    }
    let body: unknown;
    try {
      body = rawBody.length > 0 ? JSON.parse(rawBody) : undefined;
    } catch {
      body = rawBody;
    }
    const recorded: RecordedRequest = {
      method: req.method ?? '',
      url: req.url ?? '',
      headers,
      body,
    };
    requests.push(recorded);
    handler(recorded, res);
  });
  await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve));
  const address = server.address();
  if (address === null || typeof address === 'string') {
    throw new Error('mock server address unavailable');
  }
  return {
    baseUrl: `http://127.0.0.1:${address.port}`,
    requests,
    close: () =>
      new Promise<void>((resolve, reject) =>
        server.close((err) => (err ? reject(err) : resolve())),
      ),
  };
}

export function json(res: ServerResponse, status: number, body: unknown): void {
  res.writeHead(status, { 'Content-Type': 'application/json' });
  res.end(JSON.stringify(body));
}

export function noContent(res: ServerResponse): void {
  res.writeHead(204);
  res.end();
}

/** Uniform hostd error body, see hostd/src/api/error.rs. */
export function apiError(res: ServerResponse, status: number, message: string): void {
  json(res, status, { error: { code: status, message } });
}
