// Typed fetch layer for the webapp's /api/demo endpoints. All errors come
// back as Error with the server's message (uniform {error:{message}} body).

import type {
  ExecResult,
  LambdaDetail,
  LambdaInvokeResult,
  Overview,
  Project,
} from './types';

async function call<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(path, {
    headers: { 'Content-Type': 'application/json' },
    ...init,
  });
  if (res.status === 204) {
    return undefined as T;
  }
  if (!res.ok) {
    let message = `${res.status} ${res.statusText}`;
    try {
      const body = (await res.json()) as { error?: { message?: string } };
      if (body.error?.message) {
        message = body.error.message;
      }
    } catch {
      // keep the status-line fallback
    }
    throw new Error(message);
  }
  return (await res.json()) as T;
}

export const api = {
  overview: () => call<Overview>('/api/demo/overview'),

  createProject: (name: string) =>
    call<Project>('/api/demo/projects', {
      method: 'POST',
      body: JSON.stringify({ name }),
    }),

  deleteProject: (id: number) =>
    call<void>(`/api/demo/projects/${id}`, { method: 'DELETE' }),

  createVm: (projectId: number, body: { name: string; image: string }) =>
    call<Project>(`/api/demo/projects/${projectId}/vms`, {
      method: 'POST',
      body: JSON.stringify(body),
    }),

  deleteVm: (vmId: string) =>
    call<void>(`/api/demo/vms/${vmId}`, { method: 'DELETE' }),

  connectionString: (vmId: string) =>
    call<{ connectionString: string; expiresAt: string }>(
      `/api/demo/vms/${vmId}/connection-string`,
      { method: 'POST' },
    ),

  branch: (vmId: string, name: string) =>
    call<Project>(`/api/demo/vms/${vmId}/branch`, {
      method: 'POST',
      body: JSON.stringify({ name }),
    }),

  exec: (vmId: string, cmd: string) =>
    call<ExecResult>(`/api/demo/vms/${vmId}/exec`, {
      method: 'POST',
      body: JSON.stringify({ cmd }),
    }),

  sql: (vmId: string, sql: string) =>
    call<ExecResult>(`/api/demo/vms/${vmId}/sql`, {
      method: 'POST',
      body: JSON.stringify({ sql }),
    }),

  createLambda: (projectId: number, body: { name: string; language: string }) =>
    call<Project>(`/api/demo/projects/${projectId}/lambdas`, {
      method: 'POST',
      body: JSON.stringify(body),
    }),

  getLambda: (vmId: string) => call<LambdaDetail>(`/api/demo/vms/${vmId}/lambda`),

  saveLambda: (vmId: string, source: string) =>
    call<{ ok: boolean }>(`/api/demo/vms/${vmId}/lambda`, {
      method: 'PUT',
      body: JSON.stringify({ source }),
    }),

  // The lambda's reply is an arbitrary body, not the uniform JSON shape —
  // bypass `call` and surface status/body/duration as-is.
  invokeLambda: async (slug: string, body: string): Promise<LambdaInvokeResult> => {
    const started = Date.now();
    const res = await fetch(`/api/demo/f/${slug}`, {
      method: 'POST',
      headers: { 'Content-Type': 'text/plain' },
      body,
    });
    return {
      status: res.status,
      body: await res.text(),
      durationMs: Number(res.headers.get('x-lambda-duration-ms')) || Date.now() - started,
    };
  },
};
