// Types mirroring the webapp server's DTOs (server/src/routes.ts).

export type ProjectStatus = 'provisioning' | 'ready' | 'error' | 'deleting';
export type VmKind = 'tiko' | 'extra' | 'lambda' | 'postgrest';
export type LambdaLanguage = 'node' | 'python';
export type LambdaStatus = 'deploying' | 'ready' | 'error';

export interface LambdaSummary {
  slug: string;
  language: LambdaLanguage;
  status: LambdaStatus;
  step: string;
  error: string | null;
}

export interface PostgrestSummary {
  slug: string;
  status: LambdaStatus;
  step: string;
  error: string | null;
}

/** GET /vms/:vmId/postgrest — summary plus the API base path. */
export interface PostgrestDetail extends PostgrestSummary {
  apiBase: string;
}

/** GET /vms/:vmId/lambda — summary plus the deployed source. */
export interface LambdaDetail extends LambdaSummary {
  source: string;
  invokePath: string;
}

export interface LambdaInvokeResult {
  status: number;
  body: string;
  durationMs: number;
}

export interface ProjectVm {
  vmId: string;
  name: string;
  image: string;
  kind: VmKind;
  lambda?: LambdaSummary;
  postgrest?: PostgrestSummary;
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
  lambda?: LambdaSummary;
  postgrest?: PostgrestSummary;
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

/** Plain extra-VM images. node-22/python-3.12 are intentionally not here —
 * they are only offered as lambdas (LAMBDA_OPTIONS below). */
export const EXTRA_IMAGES = ['ubuntu-24'] as const;

/** Pseudo-images for the add-VM select: `lambda:<language>` values are
 * routed to the lambda-create endpoint instead of plain VM creation. */
export const LAMBDA_OPTIONS = [
  { value: 'lambda:node', label: 'node-22 (λ lambda)' },
  { value: 'lambda:python', label: 'python-3.12 (λ lambda)' },
  { value: 'postgrest', label: 'postgrest (REST API)' },
] as const;
