import { HttpClient } from './http.js';
import type { EnvVar, WorkloadLogEntry, WorkloadOrigin, WorkloadSpec, WorkloadState } from './types.js';

/** Raw wire shape of a workload as returned by hostd (see hostd/src/vmm/workload.rs). */
export interface WorkloadData {
  workload_id: string;
  vm_id: string;
  spec: WorkloadSpec;
  state: WorkloadState;
  origin: WorkloadOrigin;
  pid: number | null;
  exit_code: number | null;
  signal: number | null;
  created_at: string;
  started_at: string | null;
  finished_at: string | null;
}

/** Options for starting a workload from a bare command (see WorkloadSpec). */
export interface StartWorkloadOptions {
  env?: EnvVar[];
  cwd?: string;
}

/** Response of POST /api/vms/{id}/exec: the finished workload (wire shape) plus its captured logs. */
export interface ExecResult extends WorkloadData {
  logs: WorkloadLogEntry[];
}

export interface WaitOptions {
  /** How long to poll before giving up; defaults to 60s. */
  timeoutMs?: number;
  /** Poll interval; defaults to 500ms. */
  intervalMs?: number;
}

/**
 * A single workload inside a VM: a resource wrapper bound to a
 * `workload_id` that caches the latest state. The public fields mirror the
 * hostd wire shape; the methods call hostd and update the cache.
 */
export class Workload {
  /** @internal */
  readonly http: HttpClient;
  readonly vmId: string;
  readonly workload_id: string;
  spec: WorkloadSpec;
  state: WorkloadState;
  readonly origin: WorkloadOrigin;
  pid: number | null;
  exit_code: number | null;
  signal: number | null;
  readonly created_at: string;
  started_at: string | null;
  finished_at: string | null;

  /** @internal */
  constructor(http: HttpClient, data: WorkloadData) {
    this.http = http;
    this.vmId = data.vm_id;
    this.workload_id = data.workload_id;
    this.spec = data.spec;
    this.state = data.state;
    this.origin = data.origin;
    this.pid = data.pid;
    this.exit_code = data.exit_code;
    this.signal = data.signal;
    this.created_at = data.created_at;
    this.started_at = data.started_at;
    this.finished_at = data.finished_at;
  }

  get id(): string {
    return this.workload_id;
  }

  get isActive(): boolean {
    return this.state === 'starting' || this.state === 'running';
  }

  /** Re-fetch the workload's current state from hostd. */
  async refresh(): Promise<this> {
    this.apply(
      await this.http.request<WorkloadData>({
        method: 'GET',
        path: `/api/vms/${this.vmId}/workloads/${this.workload_id}`,
      }),
    );
    return this;
  }

  /** Stop a running workload (SIGTERM, escalating to SIGKILL in the guest). */
  async stop(): Promise<this> {
    this.apply(
      await this.http.request<WorkloadData>({
        method: 'POST',
        path: `/api/vms/${this.vmId}/workloads/${this.workload_id}/stop`,
      }),
    );
    return this;
  }

  /** Fetch the workload's captured stdout/stderr, in arrival order. */
  async logs(): Promise<WorkloadLogEntry[]> {
    return this.http.request<WorkloadLogEntry[]>({
      method: 'GET',
      path: `/api/vms/${this.vmId}/workloads/${this.workload_id}/logs`,
    });
  }

  /**
   * Poll until the workload reaches a terminal state (`exited`, `stopped`
   * or `failed`), refreshing the cache along the way. Useful after `start()`
   * for short commands.
   */
  async wait(options: WaitOptions = {}): Promise<this> {
    const timeoutMs = options.timeoutMs ?? 60_000;
    const intervalMs = options.intervalMs ?? 500;
    const deadline = Date.now() + timeoutMs;
    while (this.isActive) {
      if (Date.now() >= deadline) {
        throw new Error(
          `workload ${this.workload_id} did not reach a terminal state within ${timeoutMs}ms ` +
            `(state: ${this.state})`,
        );
      }
      await sleep(intervalMs);
      await this.refresh();
    }
    return this;
  }

  /** @internal */
  apply(data: WorkloadData): void {
    this.state = data.state;
    this.spec = data.spec;
    this.pid = data.pid;
    this.exit_code = data.exit_code;
    this.signal = data.signal;
    this.started_at = data.started_at;
    this.finished_at = data.finished_at;
  }
}

/**
 * The `/api/vms/{id}/workloads` resource: start commands inside the VM via
 * guestd and inspect their run state and captured logs.
 */
export class WorkloadsApi {
  /** @internal */
  constructor(
    private readonly http: HttpClient,
    private readonly vmId: string,
  ) {}

  /** List all workloads of the VM. */
  async list(): Promise<Workload[]> {
    const workloads = await this.http.request<WorkloadData[]>({
      method: 'GET',
      path: `/api/vms/${this.vmId}/workloads`,
    });
    return workloads.map((data) => new Workload(this.http, data));
  }

  /** Start a workload; accepts a full `WorkloadSpec` or a bare command. */
  async start(spec: WorkloadSpec): Promise<Workload>;
  async start(cmd: string[], options?: StartWorkloadOptions): Promise<Workload>;
  async start(
    specOrCmd: WorkloadSpec | string[],
    options: StartWorkloadOptions = {},
  ): Promise<Workload> {
    const spec: WorkloadSpec = Array.isArray(specOrCmd)
      ? {
          cmd: specOrCmd,
          ...(options.env !== undefined ? { env: options.env } : {}),
          ...(options.cwd !== undefined ? { cwd: options.cwd } : {}),
        }
      : specOrCmd;
    const data = await this.http.request<WorkloadData>({
      method: 'POST',
      path: `/api/vms/${this.vmId}/workloads`,
      body: spec,
    });
    return new Workload(this.http, data);
  }

  /** Fetch a single workload by id. */
  async get(workloadId: string): Promise<Workload> {
    const data = await this.http.request<WorkloadData>({
      method: 'GET',
      path: `/api/vms/${this.vmId}/workloads/${workloadId}`,
    });
    return new Workload(this.http, data);
  }

  /** Stop a running workload; returns the updated workload. */
  async stop(workloadId: string): Promise<Workload> {
    const data = await this.http.request<WorkloadData>({
      method: 'POST',
      path: `/api/vms/${this.vmId}/workloads/${workloadId}/stop`,
    });
    return new Workload(this.http, data);
  }

  /** Fetch a workload's captured stdout/stderr, in arrival order. */
  async logs(workloadId: string): Promise<WorkloadLogEntry[]> {
    return this.http.request<WorkloadLogEntry[]>({
      method: 'GET',
      path: `/api/vms/${this.vmId}/workloads/${workloadId}/logs`,
    });
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => {
    setTimeout(resolve, ms);
  });
}
