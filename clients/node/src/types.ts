// Types mirroring the hostd HTTP API's serde JSON shapes
// (see hostd/src/vmm/vm.rs, hostd/src/vmm/workload.rs, hostd/src/net/types.rs).
// Field names are snake_case to match the wire format exactly.

export interface EnvVar {
  key: string;
  value: string;
}

export type VmMode = 'ephemeral' | 'permanent' | 'schedule';

export type VmState =
  // transitional
  | 'creating'
  | 'starting'
  | 'pausing'
  | 'resuming'
  | 'suspending'
  | 'restoring'
  | 'destroying'
  // stable
  | 'created'
  | 'started'
  | 'paused'
  | 'suspended'
  | 'destroyed';

/** Known guest images. Any string is accepted (forward compat); the union is for autocomplete. */
export type VmImage =
  | 'ubuntu-24'
  | 'python-3.12'
  | 'node-22'
  | 'postgres-16'
  | 's3files'
  | 'tiko-postgres'
  | (string & {});

export interface ExposedPort {
  port: number;
  label: string;
}

export interface NetworkConfig {
  allow_internet: boolean;
  exposed_ports: ExposedPort[];
  egress: string[];
  public_access: boolean;
}

export interface VmNet {
  tap_name: string;
  guest_ip: string;
  gateway_ip: string;
  /** CIDR string, e.g. `172.16.0.0/24`. */
  subnet: string;
  guest_mac: string;
}

/** Auto-suspend config as hostd stores and returns it (all fields resolved). */
export interface AutoSuspendConfig {
  idle_timeout_secs: number;
  idle_check_cmd: string[];
  check_interval_secs: number;
}

/**
 * Create-VM form of AutoSuspendConfig: only `idle_timeout_secs` is required;
 * hostd defaults `idle_check_cmd` to [] (overridden again to the image's
 * SQL-based check for postgres-16/tiko-postgres VMs) and
 * `check_interval_secs` to 30.
 */
export interface AutoSuspendCreateConfig {
  idle_timeout_secs: number;
  idle_check_cmd?: string[];
  check_interval_secs?: number;
}

export interface BlockStorageConfig {
  size_mb: number;
  chunk_kb: number | null;
  mount_path: string;
}

export interface VmConfig {
  name: string;
  project_id: number;
  mode: VmMode;
  image: string;
  cpus: number;
  memory_mb: number;
  disk_size_mb: number;
  network_config: NetworkConfig;
  ssh_access: boolean;
  env: EnvVar[];
  cmd: string[];
  services: string[];
  cron_schedule: string | null;
  timeout_secs: number | null;
  tags: string[];
  auto_suspend: AutoSuspendConfig | null;
  block_storage: BlockStorageConfig | null;
}

export interface VmSnapshot {
  state_path: string;
  mem_path: string;
  created_at: string;
}

export interface VmInstance {
  vm_id: string;
  state: VmState;
  work_dir: string;
  socket_path: string;
  kernel_path: string;
  initramfs_path: string;
  boot_args: string;
  rootfs_path: string;
  overlay_disk: string;
  block_device: string | null;
  net: VmNet | null;
  guest_cid: number | null;
  vsock_uds_path: string;
  snapshot: VmSnapshot | null;
  serial_log: string;
  error_log: string;
  created_at: string;
  vm_config: VmConfig;
}

/** Create-VM request body (see VmConfig in hostd/src/vmm/vm.rs). */
export interface VmCreateConfig {
  name: string;
  project_id: number;
  image: VmImage;
  mode?: VmMode;
  cpus?: number;
  memory_mb?: number;
  disk_size_mb?: number;
  network_config?: Partial<NetworkConfig>;
  ssh_access?: boolean;
  env?: EnvVar[];
  cmd?: string[];
  services?: string[];
  cron_schedule?: string | null;
  timeout_secs?: number | null;
  tags?: string[];
  auto_suspend?: AutoSuspendCreateConfig | null;
  block_storage?: BlockStorageConfig | null;
}

export interface CreateVmResponse {
  status: 'created';
  payload: VmConfig;
  id: string;
}

export interface WorkloadSpec {
  cmd: string[];
  env?: EnvVar[];
  cwd?: string | null;
}

export type WorkloadState = 'starting' | 'running' | 'exited' | 'stopped' | 'failed';

export type WorkloadOrigin = 'api' | 'schedule';

export interface WorkloadLogEntry {
  ts: string;
  stream: string;
  data: string;
}

export interface HealthResponse {
  status: string;
}
