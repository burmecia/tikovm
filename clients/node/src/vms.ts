import { HttpClient } from './http.js';
import { NetworkApi } from './network.js';
import { PortsApi } from './ports.js';
import { WorkloadsApi } from './workload.js';
import type {
  CreateVmResponse,
  VmConfig,
  VmCreateConfig,
  VmInstance,
  VmSnapshot,
} from './types.js';
import { Vm } from './vm.js';

/** The `/api/vms` resource: VM lifecycle management. */
export class VmsApi {
  constructor(private readonly http: HttpClient) {}

  /** List all VMs across all projects. */
  async list(): Promise<Vm[]> {
    const instances = await this.http.request<VmInstance[]>({ method: 'GET', path: '/api/vms' });
    return instances.map((data) => new Vm(this.http, data.vm_id, data));
  }

  /** Fetch a single VM by id. */
  async get(id: string): Promise<Vm> {
    const data = await this.http.request<VmInstance>({ method: 'GET', path: `/api/vms/${id}` });
    return new Vm(this.http, data.vm_id, data);
  }

  /**
   * Create and start a VM. hostd requires many config fields that are
   * tedious to spell out (`mode`, `cpus`, `memory_mb`, `disk_size_mb`,
   * `network_config`, `ssh_access`); the client fills sensible defaults for
   * any that are omitted. Returns a `Vm` pre-seeded with the echoed config;
   * call `vm.refresh()` for live state.
   */
  async create(config: VmCreateConfig): Promise<Vm> {
    const response = await this.http.request<CreateVmResponse>({
      method: 'POST',
      path: '/api/vms',
      body: toVmConfig(config),
    });
    const vm = new Vm(this.http, response.id);
    vm.seedConfig(response.payload);
    return vm;
  }

  /** Delete a VM. */
  async delete(id: string): Promise<void> {
    await this.http.request<void>({ method: 'DELETE', path: `/api/vms/${id}` });
  }

  /** Pause a running VM; returns a `Vm` with the fresh state. */
  async pause(id: string): Promise<Vm> {
    const data = await this.http.request<VmInstance>({
      method: 'POST',
      path: `/api/vms/${id}/pause`,
    });
    return new Vm(this.http, data.vm_id, data);
  }

  /** Resume a paused VM; returns a `Vm` with the fresh state. */
  async resume(id: string): Promise<Vm> {
    const data = await this.http.request<VmInstance>({
      method: 'POST',
      path: `/api/vms/${id}/resume`,
    });
    return new Vm(this.http, data.vm_id, data);
  }

  /** Take a snapshot of a VM, leaving it suspended. */
  async snapshot(id: string): Promise<VmSnapshot> {
    return this.http.request<VmSnapshot>({ method: 'POST', path: `/api/vms/${id}/snapshot` });
  }

  /** Restore a suspended VM from its snapshot. */
  async restore(id: string): Promise<Vm> {
    const data = await this.http.request<VmInstance>({
      method: 'POST',
      path: `/api/vms/${id}/restore`,
    });
    return new Vm(this.http, data.vm_id, data);
  }

  /** Read-only network config of a VM by id. */
  network(id: string): NetworkApi {
    return new NetworkApi(this.http, id);
  }

  /** Exposed-port registry and proxy-token minting of a VM by id. */
  ports(id: string): PortsApi {
    return new PortsApi(this.http, id);
  }

  /** Workloads (commands run inside the guest) of a VM by id. */
  workloads(id: string): WorkloadsApi {
    return new WorkloadsApi(this.http, id);
  }
}

/**
 * Fill hostd's mandatory, non-defaulted config fields with sensible defaults
 * so callers can `create({ name, project_id, image, ... })` (see VmConfig in
 * hostd/src/vmm/vm.rs — `mode`, `cpus`, `memory_mb`, `disk_size_mb`,
 * `network_config` and `ssh_access` have no serde default and must be sent).
 */
export function toVmConfig(config: VmCreateConfig): VmConfig {
  return {
    name: config.name,
    project_id: config.project_id,
    image: config.image,
    mode: config.mode ?? 'ephemeral',
    cpus: config.cpus ?? 1,
    memory_mb: config.memory_mb ?? 512,
    disk_size_mb: config.disk_size_mb ?? 1024,
    network_config: {
      allow_internet: config.network_config?.allow_internet ?? false,
      exposed_ports: config.network_config?.exposed_ports ?? [],
      egress: config.network_config?.egress ?? [],
      public_access: config.network_config?.public_access ?? false,
    },
    ssh_access: config.ssh_access ?? false,
    env: config.env ?? [],
    cmd: config.cmd ?? [],
    services: config.services ?? [],
    cron_schedule: config.cron_schedule ?? null,
    timeout_secs: config.timeout_secs ?? null,
    tags: config.tags ?? [],
    auto_suspend: config.auto_suspend ?? null,
    block_storage: config.block_storage ?? null,
  };
}
