//! Webapp configuration, loaded from the environment at startup.
//!
//! The webapp sits between the browser and hostd: it holds the hostd Bearer
//! token (the browser never sees it) and owns the demo policy knobs (project
//! TTL, tiko org id, the tag that marks every VM this app creates).

export interface Config {
  /** Port the webapp's own HTTP server listens on. */
  port: number;
  /** Base URL of the hostd API server (the listener serving `/api`). */
  hostdUrl: string;
  /** Bearer token hostd requires on every request. */
  hostdToken: string;
  /** Lifetime of a demo project (and its VMs) in milliseconds. */
  projectTtlMs: number;
  /** Tag stamped on every VM this app creates (orphan sweep + cleanup). */
  demoTag: string;
  /** Fixed Tiko organization id baked into every per-project tiko.env. */
  orgId: number;
}

function intEnv(value: string | undefined, fallback: number): number {
  if (value === undefined || value.trim() === '') {
    return fallback;
  }
  const n = Number.parseInt(value, 10);
  if (!Number.isFinite(n) || n <= 0) {
    throw new Error(`invalid numeric env value: ${value}`);
  }
  return n;
}

export function loadConfig(env: NodeJS.ProcessEnv = process.env): Config {
  const hostdToken = env.TIKOVM_HOSTD_API_TOKEN ?? '';
  if (!hostdToken.trim()) {
    throw new Error(
      'TIKOVM_HOSTD_API_TOKEN must be set to a non-empty hostd Bearer token',
    );
  }
  return {
    port: intEnv(env.PORT, 4000),
    hostdUrl: (env.HOSTD_URL ?? 'http://127.0.0.1:3000').replace(/\/+$/, ''),
    hostdToken,
    projectTtlMs: intEnv(env.PROJECT_TTL_MS, 60 * 60 * 1000),
    demoTag: env.DEMO_TAG ?? 'tikovm-demo',
    orgId: intEnv(env.TIKO_ORG_ID, 12),
  };
}
