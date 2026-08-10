import { TikovmApiError, TikovmProtocolError, TikovmRequestError } from './errors.js';

export interface RequestOptions {
  method: string;
  path: string;
  body?: unknown;
}

interface ErrorBody {
  error?: { code?: unknown; message?: unknown };
}

/**
 * Minimal JSON-over-HTTP client for the hostd API: Bearer-token auth, uniform
 * `{error:{code,message}}` error parsing, 204 -> no content. Uses the native
 * global `fetch` (Node >= 18), so there are no runtime dependencies.
 */
export class HttpClient {
  readonly baseUrl: string;
  private readonly token: string;

  constructor(baseUrl: string, token: string) {
    this.baseUrl = baseUrl;
    this.token = token;
  }

  async request<T>(options: RequestOptions): Promise<T> {
    const url = `${this.baseUrl}${options.path}`;
    const headers: Record<string, string> = {
      Accept: 'application/json',
      Authorization: `Bearer ${this.token}`,
    };
    let body: string | undefined;
    if (options.body !== undefined) {
      headers['Content-Type'] = 'application/json';
      body = JSON.stringify(options.body);
    }

    let response: Response;
    try {
      const init: RequestInit = { method: options.method, headers };
      if (body !== undefined) {
        init.body = body;
      }
      response = await fetch(url, init);
    } catch (cause) {
      throw new TikovmRequestError(url, `request to ${url} failed: ${describe(cause)}`, cause);
    }

    if (!response.ok) {
      throw await apiErrorFrom(response);
    }
    if (response.status === 204) {
      return undefined as T;
    }
    const text = await response.text();
    if (text.length === 0) {
      throw new TikovmProtocolError(`expected a JSON body from ${url} but got an empty response`);
    }
    try {
      return JSON.parse(text) as T;
    } catch (cause) {
      throw new TikovmProtocolError(`invalid JSON response from ${url}: ${describe(cause)}`, {
        cause,
      });
    }
  }
}

async function apiErrorFrom(response: Response): Promise<TikovmApiError> {
  let body: ErrorBody | undefined;
  try {
    body = (await response.json()) as ErrorBody;
  } catch {
    body = undefined;
  }
  const error = body?.error;
  const code = typeof error?.code === 'number' ? error.code : response.status;
  const message =
    typeof error?.message === 'string' ? error.message : `HTTP ${response.status}`;
  return new TikovmApiError(response.status, code, message);
}

function describe(cause: unknown): string {
  if (cause instanceof Error) {
    return cause.message;
  }
  return String(cause);
}
