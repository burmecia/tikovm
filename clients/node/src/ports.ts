import { HttpClient } from './http.js';
import type { ExposedPort } from './types.js';

/**
 * Forwarding mode a proxy token is valid for (hostd's `proxy::Proto`).
 * `http` reverse-proxies requests; `tcp` splices raw bytes instead (e.g. the
 * Postgres wire protocol, where the JWT rides in the `tikovm_token` startup
 * parameter).
 */
export type ProxyProto = 'http' | 'tcp';

export interface MintPortTokenOptions {
  /** Requested token lifetime in seconds; hostd clamps it to its maximum. */
  ttl_secs?: number;
  /** Forwarding mode the token is valid for; defaults to `http`. */
  proto?: ProxyProto;
}

/** Response of POST `/api/vms/{id}/ports/{port}/token`. */
export interface PortToken {
  token: string;
  expires_at: string;
}

/**
 * The `/api/vms/{id}/ports` resource: the per-VM registry of exposed guest
 * ports (with labels) plus minting of the ephemeral JWTs the proxy server
 * uses to authenticate forwarded requests. Unexposing a port revokes proxy
 * access immediately (hostd re-validates it on every connection).
 */
export class PortsApi {
  /** @internal */
  constructor(
    private readonly http: HttpClient,
    private readonly vmId: string,
  ) {}

  /** List the VM's exposed ports. */
  async list(): Promise<ExposedPort[]> {
    return this.http.request<ExposedPort[]>({
      method: 'GET',
      path: `/api/vms/${this.vmId}/ports`,
    });
  }

  /** Expose a guest port, with a label describing its purpose. */
  async expose(port: ExposedPort): Promise<ExposedPort> {
    return this.http.request<ExposedPort>({
      method: 'POST',
      path: `/api/vms/${this.vmId}/ports`,
      body: port,
    });
  }

  /** Stop exposing a port by number; proxy access to it is revoked immediately. */
  async remove(port: number): Promise<void> {
    await this.http.request<void>({
      method: 'DELETE',
      path: `/api/vms/${this.vmId}/ports/${port}`,
    });
  }

  /**
   * Mint an ephemeral JWT authorizing proxy requests to this exposed port.
   * Requires the port to be currently exposed.
   */
  async token(port: number, options: MintPortTokenOptions = {}): Promise<PortToken> {
    const body: MintPortTokenOptions = {};
    if (options.ttl_secs !== undefined) {
      body.ttl_secs = options.ttl_secs;
    }
    if (options.proto !== undefined) {
      body.proto = options.proto;
    }
    return this.http.request<PortToken>({
      method: 'POST',
      path: `/api/vms/${this.vmId}/ports/${port}/token`,
      body,
    });
  }
}
