import { HttpClient } from './http.js';
import type { HealthResponse } from './types.js';
import { VmsApi } from './vms.js';

export interface TikovmOptions {
  /**
   * Bearer token hostd requires on every request (hostd refuses to start
   * without a non-empty `TIKOVM_HOSTD_API_TOKEN`).
   */
  accessToken: string;
  /**
   * Base URL of the hostd API server (the listener that serves `/api`),
   * without a trailing `/api` or slash. Defaults to `http://localhost:3000`.
   */
  baseUrl?: string;
}

/** Client for the hostd HTTP API. */
export class Tikovm {
  /** VM lifecycle management (`list`, `get`, `create`, `delete`, ...). */
  readonly vms: VmsApi;
  private readonly http: HttpClient;

  constructor(options: TikovmOptions) {
    const { accessToken, baseUrl = 'http://localhost:3000' } = options;
    if (typeof accessToken !== 'string' || accessToken.trim().length === 0) {
      throw new TypeError('Tikovm requires a non-empty accessToken');
    }
    if (typeof baseUrl !== 'string' || baseUrl.length === 0) {
      throw new TypeError('Tikovm requires a non-empty baseUrl');
    }
    this.http = new HttpClient(baseUrl.replace(/\/+$/, ''), accessToken);
    this.vms = new VmsApi(this.http);
  }

  /** Liveness probe: `{ status: 'ok' }` when hostd is reachable. */
  async health(): Promise<HealthResponse> {
    return this.http.request<HealthResponse>({ method: 'GET', path: '/api/health' });
  }
}
