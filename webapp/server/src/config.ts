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
  /** Host written into the psql connection string (the address users reach
   * hostd's proxy listener on from outside). Empty = auto-detect at startup
   * (EC2 public IPv4, falling back to the HOSTD_URL hostname). */
  proxyHost: string;
  /** Port of hostd's proxy listener. */
  proxyPort: number;
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
  const hostdUrl = (env.HOSTD_URL ?? 'http://127.0.0.1:3000').replace(/\/+$/, '');
  return {
    port: intEnv(env.PORT, 4000),
    hostdUrl,
    hostdToken,
    projectTtlMs: intEnv(env.PROJECT_TTL_MS, 60 * 60 * 1000),
    demoTag: env.DEMO_TAG ?? 'tikovm-demo',
    orgId: intEnv(env.TIKO_ORG_ID, 12),
    proxyHost: env.PROXY_HOST ?? '',
    proxyPort: intEnv(env.PROXY_PORT, 8080),
  };
}

/**
 * Best-effort public IPv4 of this instance via the EC2 metadata service
 * (IMDSv2, with an IMDSv1 fallback); null when metadata is unreachable or
 * the instance has no public IP — callers fall back to a configured value.
 * The connection string the UI hands out is used from *outside* this
 * machine, so the public address is the right default here.
 */
export async function detectPublicIpv4(timeoutMs = 1000): Promise<string | null> {
  const base = 'http://169.254.169.254/latest';
  try {
    const tokenRes = await fetch(`${base}/api/token`, {
      method: 'PUT',
      headers: { 'X-aws-ec2-metadata-token-ttl-seconds': '60' },
      signal: AbortSignal.timeout(timeoutMs),
    });
    // IMDSv1 fallback: no token header when the PUT didn't produce one.
    const headers: Record<string, string> = {};
    if (tokenRes.ok) {
      headers['X-aws-ec2-metadata-token'] = await tokenRes.text();
    }
    const res = await fetch(`${base}/meta-data/public-ipv4`, {
      headers,
      signal: AbortSignal.timeout(timeoutMs),
    });
    if (res.ok) {
      const ip = (await res.text()).trim();
      if (/^\d{1,3}(\.\d{1,3}){3}$/.test(ip)) {
        return ip;
      }
    }
  } catch {
    // not on EC2, or IMDS unreachable/disabled
  }
  return null;
}
