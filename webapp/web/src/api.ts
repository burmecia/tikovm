// Typed fetch layer for the webapp's /api/demo endpoints. All errors come
// back as Error with the server's message (uniform {error:{message}} body).

import type { ExecResult, Overview, Project } from './types';

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
};
