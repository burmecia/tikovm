// Types mirroring the webapp server's DTOs (server/src/routes.ts).

export type ProjectStatus = 'provisioning' | 'ready' | 'error' | 'deleting';
export type VmKind = 'tiko' | 'extra';

export interface ProjectVm {
  vmId: string;
  name: string;
  image: string;
  kind: VmKind;
}

export interface Project {
  id: number;
  dbId: number;
  name: string;
  status: ProjectStatus;
  step: string;
  error: string | null;
  createdAt: string;
  expiresInSeconds: number;
  /** Set when this project's database branched from another project's. */
  branchedFrom: { projectId: number; dbId: number } | null;
  vms: ProjectVm[];
}

export interface OverviewVm {
  vmId: string;
  name: string;
  projectId: number;
  image: string;
  kind: VmKind;
  state: string;
  guestIp: string | null;
  cpus: number;
  memoryMb: number;
  createdAt: string;
}

export interface Overview {
  hostdReachable: boolean;
  projects: Project[];
  vms: OverviewVm[];
}

export interface ExecResult {
  exitCode: number | null;
  state: string;
  output: string;
}

export const EXTRA_IMAGES = ['ubuntu-24', 'python-3.12', 'node-22'] as const;
