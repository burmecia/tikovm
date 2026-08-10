import { HttpClient } from './http.js';
import type {
  EnvVar,
  ExecResult,
  VmConfig,
  VmInstance,
  VmNet,
  VmSnapshot,
  VmState,
  WorkloadSpec,
} from './types.js';

export interface ExecOptions {
  env?: EnvVar[];
  cwd?: string;
}

/**
 * A single VM: a resource wrapper bound to a `vm_id` that caches the live
 * `VmInstance` returned by the API. Every lifecycle method calls hostd and
 * updates the cache, so chained calls see fresh state without an explicit
 * `refresh()`.
 */
export class Vm {
  private data: VmInstance | undefined;
  private seededConfig: VmConfig | undefined;

  /** @internal */
  constructor(
    private readonly http: HttpClient,
    readonly id: string,
    data?: VmInstance,
  ) {
    this.data = data;
  }

  /** @internal Seeds config from a create response before the first refresh. */
  seedConfig(config: VmConfig): void {
    this.seededConfig = config;
  }

  /** Last known configuration of the VM (from the create response or latest fetch). */
  get vmConfig(): VmConfig | undefined {
    return this.data?.vm_config ?? this.seededConfig;
  }

  /** Last known state; `undefined` until the VM has been fetched or acted on. */
  get state(): VmState | undefined {
    return this.data?.state;
  }

  /** Last known network identity; `undefined` until fetched. */
  get net(): VmNet | undefined {
    return this.data?.net ?? undefined;
  }

  get isRunning(): boolean {
    return this.state === 'started';
  }

  get isPaused(): boolean {
    return this.state === 'paused';
  }

  get isSuspended(): boolean {
    return this.state === 'suspended';
  }

  get isDestroyed(): boolean {
    return this.state === 'destroyed';
  }

  /** Re-fetch the VM's current state from hostd. */
  async refresh(): Promise<this> {
    this.data = await this.http.request<VmInstance>({
      method: 'GET',
      path: `/api/vms/${this.id}`,
    });
    return this;
  }

  /** Pause a running VM. */
  async pause(): Promise<this> {
    this.data = await this.http.request<VmInstance>({
      method: 'POST',
      path: `/api/vms/${this.id}/pause`,
    });
    return this;
  }

  /** Resume a paused VM. */
  async resume(): Promise<this> {
    this.data = await this.http.request<VmInstance>({
      method: 'POST',
      path: `/api/vms/${this.id}/resume`,
    });
    return this;
  }

  /** Take a snapshot, leaving the VM suspended. Returns the snapshot paths. */
  async snapshot(): Promise<VmSnapshot> {
    const snapshot = await this.http.request<VmSnapshot>({
      method: 'POST',
      path: `/api/vms/${this.id}/snapshot`,
    });
    await this.refresh();
    return snapshot;
  }

  /** Restore a suspended VM from its snapshot. */
  async restore(): Promise<this> {
    this.data = await this.http.request<VmInstance>({
      method: 'POST',
      path: `/api/vms/${this.id}/restore`,
    });
    return this;
  }

  /** Destroy the VM. The wrapper is left with no cached state. */
  async delete(): Promise<void> {
    await this.http.request<void>({ method: 'DELETE', path: `/api/vms/${this.id}` });
    this.data = undefined;
    this.seededConfig = undefined;
  }

  /**
   * Run a command inside the VM and block until it exits (hostd's
   * synchronous `/exec` wrapper over the workloads API). Returns the
   * finished workload plus its captured stdout/stderr logs.
   */
  async exec(cmd: string[], options: ExecOptions = {}): Promise<ExecResult> {
    const spec: WorkloadSpec = {
      cmd,
      env: options.env ?? [],
      ...(options.cwd !== undefined ? { cwd: options.cwd } : {}),
    };
    return this.http.request<ExecResult>({
      method: 'POST',
      path: `/api/vms/${this.id}/exec`,
      body: spec,
    });
  }
}
