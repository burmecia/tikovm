import { HttpClient } from './http.js';
import type { NetworkConfig } from './types.js';

/**
 * Read-only `/api/vms/{id}/network` resource: the VM's live `NetworkConfig`.
 * The `exposed_ports` list is managed via the `/ports` endpoints; the
 * remaining fields are not writable through the API.
 */
export class NetworkApi {
  /** @internal */
  constructor(
    private readonly http: HttpClient,
    private readonly vmId: string,
  ) {}

  /** Fetch the VM's current network config. */
  async get(): Promise<NetworkConfig> {
    return this.http.request<NetworkConfig>({
      method: 'GET',
      path: `/api/vms/${this.vmId}/network`,
    });
  }
}
