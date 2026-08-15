//! The webapp's own REST surface, mounted under `/api/demo`.
//!
//! The browser only ever talks to these endpoints (it holds no hostd token).
//! Errors use hostd's uniform shape `{ "error": { "code", "message" } };
//! hostd failures (e.g. a 409 invalid state transition) are surfaced with
//! their original status and message.

import { Router } from 'express';
import type { Request, Response } from 'express';
import type { Tikovm } from 'tikovm';
import type { Config } from './config.js';
import {
  TikovmApiError,
  deleteVmIfExists,
  execInVm,
  logsText,
  rawListVms,
} from './hostd.js';
import {
  EXTRA_IMAGES,
  TIKO_IMAGE,
  createExtraVm,
  deleteProject,
  psqlArgv,
  provisionProject,
} from './provision.js';
import type { Project, ProjectStatus, Registry } from './state.js';

// ── DTOs (mirrored by web/src/types.ts) ─────────────────────────────────────

export interface ProjectVmDto {
  vmId: string;
  name: string;
  image: string;
  kind: 'tiko' | 'extra';
}

export interface ProjectDto {
  id: number;
  dbId: number;
  name: string;
  status: ProjectStatus;
  step: string;
  error: string | null;
  createdAt: string;
  expiresInSeconds: number;
  vms: ProjectVmDto[];
}

export interface OverviewVmDto {
  vmId: string;
  name: string;
  projectId: number;
  image: string;
  kind: 'tiko' | 'extra';
  state: string;
  guestIp: string | null;
  cpus: number;
  memoryMb: number;
  createdAt: string;
}

export interface OverviewDto {
  hostdReachable: boolean;
  projects: ProjectDto[];
  vms: OverviewVmDto[];
}

export interface ExecDto {
  exitCode: number | null;
  state: string;
  output: string;
}

// ── helpers ─────────────────────────────────────────────────────────────────

export interface ApiDeps {
  cfg: Config;
  registry: Registry;
  client: Tikovm;
}

function toProjectDto(p: Project): ProjectDto {
  return {
    id: p.id,
    dbId: p.dbId,
    name: p.name,
    status: p.status,
    step: p.step,
    error: p.error ?? null,
    createdAt: p.createdAt,
    expiresInSeconds: Math.max(0, Math.round((p.expiresAt - Date.now()) / 1000)),
    vms: p.vms.map((v) => ({ ...v })),
  };
}

function fail(res: Response, status: number, message: string): void {
  res.status(status).json({ error: { code: status, message } });
}

/** Wrap an async handler: hostd errors keep their status, others become 500. */
function handle(
  fn: (req: Request, res: Response) => Promise<void>,
): (req: Request, res: Response) => Promise<void> {
  return async (req, res) => {
    try {
      await fn(req, res);
    } catch (err) {
      if (res.headersSent) {
        return;
      }
      if (err instanceof TikovmApiError) {
        fail(res, err.status, err.message);
        return;
      }
      fail(
        res,
        500,
        err instanceof Error ? err.message : 'internal webapp error',
      );
    }
  };
}

// ── routes ──────────────────────────────────────────────────────────────────

export function apiRouter(deps: ApiDeps): Router {
  const { cfg, registry, client } = deps;
  const router = Router();

  router.get(
    '/overview',
    handle(async (_req, res) => {
      let hostdReachable = true;
      let tagged: Awaited<ReturnType<typeof rawListVms>> = [];
      try {
        const vms = await rawListVms(cfg);
        tagged = vms.filter((v) => v.vm_config.tags.includes(cfg.demoTag));
      } catch {
        hostdReachable = false; // keep serving the registry so the UI degrades
      }
      const dto: OverviewDto = {
        hostdReachable,
        projects: registry.list().map(toProjectDto),
        vms: tagged.map((v) => ({
          vmId: v.vm_id,
          name: v.vm_config.name,
          projectId: v.vm_config.project_id,
          image: v.vm_config.image,
          kind: v.vm_config.image === TIKO_IMAGE ? 'tiko' : 'extra',
          state: v.state,
          guestIp: v.net?.guest_ip ?? null,
          cpus: v.vm_config.cpus,
          memoryMb: v.vm_config.memory_mb,
          createdAt: v.created_at,
        })),
      };
      res.json(dto);
    }),
  );

  router.post(
    '/projects',
    handle(async (req, res) => {
      const name =
        typeof req.body?.name === 'string' && req.body.name.trim()
          ? req.body.name.trim().slice(0, 64)
          : null;
      const project = registry.newProject(
        name ?? `project-${registry.list().length + 1}`,
        cfg.projectTtlMs,
      );
      console.log(`[webapp] creating project ${project.id} (db ${project.dbId}) "${project.name}"`);
      // Provisioning is fully async — the UI follows project.status/step.
      void provisionProject(cfg, client, project).then(() => {
        if (project.status === 'ready') {
          console.log(`[webapp] project ${project.id} ready`);
        } else {
          console.error(`[webapp] project ${project.id} failed: ${project.error}`);
        }
      });
      res.status(202).json(toProjectDto(project));
    }),
  );

  router.delete(
    '/projects/:id',
    handle(async (req, res) => {
      const project = registry.get(Number(req.params.id));
      if (!project) {
        return fail(res, 404, `project ${req.params.id} not found`);
      }
      await deleteProject(cfg, client, registry.remove.bind(registry), project);
      console.log(`[webapp] project ${project.id} deleted`);
      res.status(204).end();
    }),
  );

  router.post(
    '/projects/:id/vms',
    handle(async (req, res) => {
      const project = registry.get(Number(req.params.id));
      if (!project) {
        return fail(res, 404, `project ${req.params.id} not found`);
      }
      if (project.status === 'deleting') {
        return fail(res, 409, `project ${project.id} is being deleted`);
      }
      if (project.status === 'provisioning') {
        return fail(res, 409, `project ${project.id} is still provisioning`);
      }
      const image = String(req.body?.image ?? '');
      if (!(EXTRA_IMAGES as readonly string[]).includes(image)) {
        return fail(res, 400, `image must be one of: ${EXTRA_IMAGES.join(', ')}`);
      }
      const name =
        typeof req.body?.name === 'string' && req.body.name.trim()
          ? req.body.name.trim().slice(0, 64)
          : `${image}-${project.vms.length + 1}`;
      const vmId = await createExtraVm(cfg, client, project, {
        name,
        image,
        cpus: typeof req.body?.cpus === 'number' ? req.body.cpus : undefined,
        memory_mb:
          typeof req.body?.memory_mb === 'number' ? req.body.memory_mb : undefined,
        disk_size_mb:
          typeof req.body?.disk_size_mb === 'number'
            ? req.body.disk_size_mb
            : undefined,
      });
      console.log(`[webapp] VM ${vmId} (${image}) added to project ${project.id}`);
      res.status(201).json(toProjectDto(project));
    }),
  );

  router.delete(
    '/vms/:vmId',
    handle(async (req, res) => {
      const vmId = req.params.vmId;
      const owner = registry.vmOwner(vmId);
      if (!owner) {
        return fail(res, 404, `VM ${vmId} is not managed by this webapp`);
      }
      // The project's tiko postgres VM goes away only with the project.
      const entry = owner.vms.find((v) => v.vmId === vmId);
      if (entry?.kind === 'tiko') {
        return fail(res, 400, 'the tiko postgres VM is deleted with its project');
      }
      await deleteVmIfExists(client, vmId);
      registry.removeVm(vmId);
      console.log(`[webapp] VM ${vmId} deleted (was in project ${owner.id})`);
      res.status(204).end();
    }),
  );

  router.post(
    '/vms/:vmId/exec',
    handle(async (req, res) => {
      const vmId = req.params.vmId;
      if (!registry.vmOwner(vmId)) {
        return fail(res, 404, `VM ${vmId} is not managed by this webapp`);
      }
      const cmd = typeof req.body?.cmd === 'string' ? req.body.cmd : '';
      if (!cmd.trim()) {
        return fail(res, 400, 'cmd must be a non-empty string');
      }
      // The user types a shell command line; run it through a login shell.
      const r = await execInVm(client, vmId, ['bash', '-lc', cmd]);
      const dto: ExecDto = {
        exitCode: r.exit_code,
        state: r.state,
        output: logsText(r),
      };
      res.json(dto);
    }),
  );

  router.post(
    '/vms/:vmId/sql',
    handle(async (req, res) => {
      const vmId = req.params.vmId;
      const owner = registry.vmOwner(vmId);
      if (!owner) {
        return fail(res, 404, `VM ${vmId} is not managed by this webapp`);
      }
      const entry = owner.vms.find((v) => v.vmId === vmId);
      if (entry?.kind !== 'tiko') {
        return fail(res, 400, 'SQL is only available on tiko postgres VMs');
      }
      const sql = typeof req.body?.sql === 'string' ? req.body.sql : '';
      if (!sql.trim()) {
        return fail(res, 400, 'sql must be a non-empty string');
      }
      const r = await execInVm(client, vmId, psqlArgv(sql));
      const dto: ExecDto = {
        exitCode: r.exit_code,
        state: r.state,
        output: logsText(r),
      };
      res.json(dto);
    }),
  );

  return router;
}
