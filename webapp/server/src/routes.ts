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
  DEFAULT_HANDLER,
  LAMBDA_IMAGES,
  LambdaNotReady,
  deploySource,
  invokeLambda,
  provisionLambda,
} from './lambda.js';
import {
  EXTRA_IMAGES,
  TIKO_IMAGE,
  TIKO_PG_PORT,
  createExtraVm,
  deleteProject,
  provisionBranch,
  psqlArgv,
  psqlConnectionString,
  provisionProject,
} from './provision.js';
import type {
  BranchOrigin,
  LambdaLanguage,
  LambdaMeta,
  Project,
  ProjectStatus,
  Registry,
} from './state.js';
import { toSlug } from './state.js';

// ── DTOs (mirrored by web/src/types.ts) ─────────────────────────────────────

/** Lambda summary embedded in VM DTOs (the source stays out of the 1s poll). */
export interface LambdaSummaryDto {
  slug: string;
  language: LambdaLanguage;
  status: LambdaMeta['status'];
  step: string;
  error: string | null;
}

export interface ProjectVmDto {
  vmId: string;
  name: string;
  image: string;
  kind: 'tiko' | 'extra' | 'lambda';
  lambda?: LambdaSummaryDto;
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
  branchedFrom: BranchOrigin | null;
  vms: ProjectVmDto[];
}

export interface OverviewVmDto {
  vmId: string;
  name: string;
  projectId: number;
  image: string;
  kind: 'tiko' | 'extra' | 'lambda';
  state: string;
  guestIp: string | null;
  cpus: number;
  memoryMb: number;
  createdAt: string;
  lambda?: LambdaSummaryDto;
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
    branchedFrom: p.branchedFrom ?? null,
    vms: p.vms.map((v) => ({ ...v, lambda: lambdaSummary(v.lambda) })),
  };
}

/** Shallow lambda summary for DTOs (drops the source). */
function lambdaSummary(
  meta: LambdaMeta | undefined,
): LambdaSummaryDto | undefined {
  if (!meta) {
    return undefined;
  }
  return {
    slug: meta.slug,
    language: meta.language,
    status: meta.status,
    step: meta.step,
    error: meta.error ?? null,
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
      if (err instanceof LambdaNotReady) {
        fail(res, 409, err.message);
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
        vms: tagged.map((v) => {
          // Kind/lambda metadata come from the registry, not the image, so
          // two VMs on the same image can be different kinds.
          const entry = registry
            .vmOwner(v.vm_id)
            ?.vms.find((e) => e.vmId === v.vm_id);
          return {
            vmId: v.vm_id,
            name: v.vm_config.name,
            projectId: v.vm_config.project_id,
            image: v.vm_config.image,
            kind: entry?.kind ?? (v.vm_config.image === TIKO_IMAGE ? 'tiko' : 'extra'),
            state: v.state,
            guestIp: v.net?.guest_ip ?? null,
            cpus: v.vm_config.cpus,
            memoryMb: v.vm_config.memory_mb,
            createdAt: v.created_at,
            lambda: lambdaSummary(entry?.lambda),
          };
        }),
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
      const cascaded = registry.descendants(project.id).length;
      await deleteProject(cfg, client, registry, project);
      console.log(
        `[webapp] project ${project.id} deleted` +
          (cascaded ? ` (cascade: ${cascaded} branch${cascaded === 1 ? '' : 'es'})` : ''),
      );
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

  router.post(
    '/vms/:vmId/branch',
    handle(async (req, res) => {
      const vmId = req.params.vmId;
      const parent = registry.vmOwner(vmId);
      if (!parent) {
        return fail(res, 404, `VM ${vmId} is not managed by this webapp`);
      }
      const entry = parent.vms.find((v) => v.vmId === vmId);
      if (entry?.kind !== 'tiko') {
        return fail(res, 400, 'only tiko postgres VMs can be branched');
      }
      // Branching reads the source database (backup) and needs it fully
      // provisioned; an error-state source would fail opaquely mid-pipeline.
      if (parent.status !== 'ready') {
        return fail(res, 409, `project ${parent.id} is not ready (status: ${parent.status})`);
      }
      const name =
        typeof req.body?.name === 'string' && req.body.name.trim()
          ? req.body.name.trim().slice(0, 64)
          : `${parent.name}-branch`;
      const project = registry.newProject(name, cfg.projectTtlMs, undefined, {
        projectId: parent.id,
        dbId: parent.dbId,
      });
      console.log(
        `[webapp] branching project ${parent.id} (db ${parent.dbId}) into ` +
          `project ${project.id} (db ${project.dbId}) "${project.name}"`,
      );
      // Provisioning (backup -> new VM -> restore -> verify) is fully async —
      // the UI follows project.status/step, same as project creation.
      void provisionBranch(cfg, client, project, parent).then(() => {
        if (project.status === 'ready') {
          console.log(`[webapp] branch project ${project.id} ready`);
        } else {
          console.error(`[webapp] branch project ${project.id} failed: ${project.error}`);
        }
      });
      res.status(202).json(toProjectDto(project));
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
    '/vms/:vmId/connection-string',
    handle(async (req, res) => {
      const vmId = req.params.vmId;
      const owner = registry.vmOwner(vmId);
      if (!owner) {
        return fail(res, 404, `VM ${vmId} is not managed by this webapp`);
      }
      const entry = owner.vms.find((v) => v.vmId === vmId);
      if (entry?.kind !== 'tiko') {
        return fail(res, 400, 'connection strings are only available for tiko postgres VMs');
      }
      // Mint a tcp proxy token (the JWT rides in the psql startup packet's
      // tikovm_token parameter). 1h matches the default project TTL.
      const { token, expires_at } = await client.vms
        .ports(vmId)
        .token(TIKO_PG_PORT, { proto: 'tcp', ttl_secs: 3600 });
      res.json({
        connectionString: psqlConnectionString(cfg, token),
        expiresAt: expires_at,
      });
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

  router.post(
    '/projects/:id/lambdas',
    handle(async (req, res) => {
      const project = registry.get(Number(req.params.id));
      if (!project) {
        return fail(res, 404, `project ${req.params.id} not found`);
      }
      // Provisioning needs the project's tiko postgres guest IP, so the
      // project must be fully up (the UI only offers this when ready).
      if (project.status !== 'ready') {
        return fail(res, 409, `project ${project.id} is not ready (status: ${project.status})`);
      }
      const name = typeof req.body?.name === 'string' ? req.body.name.trim() : '';
      if (!name) {
        return fail(res, 400, 'name must be a non-empty string');
      }
      const rawLanguage: unknown = req.body?.language;
      if (rawLanguage !== 'node' && rawLanguage !== 'python') {
        return fail(res, 400, "language must be 'node' or 'python'");
      }
      const language: LambdaLanguage = rawLanguage;
      const slug = toSlug(name);
      if (!slug) {
        return fail(res, 400, `cannot derive a URL slug from "${name}"`);
      }
      if (registry.slugTaken(slug)) {
        return fail(res, 409, `a lambda named "${slug}" already exists`);
      }
      // Placeholder vmId: filled in by provisionLambda as soon as hostd
      // answers the create call; the entry makes the deploy visible at once.
      const entry = {
        vmId: '',
        name: name.slice(0, 64),
        image: LAMBDA_IMAGES[language],
        kind: 'lambda' as const,
        lambda: {
          slug,
          language,
          status: 'deploying' as const,
          step: 'queued',
          error: undefined,
          source: DEFAULT_HANDLER[language],
        },
      };
      project.vms.push(entry);
      console.log(`[webapp] deploying lambda ${slug} (${language}) in project ${project.id}`);
      void provisionLambda(cfg, client, project, entry);
      res.status(202).json(toProjectDto(project));
    }),
  );

  router.get(
    '/vms/:vmId/lambda',
    handle(async (req, res) => {
      const entry = registry.vmOwner(req.params.vmId)?.vms.find(
        (v) => v.vmId === req.params.vmId,
      );
      if (!entry?.lambda) {
        return fail(res, 404, `VM ${req.params.vmId} is not a lambda`);
      }
      res.json({
        ...lambdaSummary(entry.lambda),
        source: entry.lambda.source,
        invokePath: `/api/demo/f/${entry.lambda.slug}`,
      });
    }),
  );

  router.put(
    '/vms/:vmId/lambda',
    handle(async (req, res) => {
      const entry = registry.vmOwner(req.params.vmId)?.vms.find(
        (v) => v.vmId === req.params.vmId,
      );
      if (!entry?.lambda) {
        return fail(res, 404, `VM ${req.params.vmId} is not a lambda`);
      }
      if (entry.lambda.status !== 'ready') {
        return fail(res, 409, `lambda is not ready (status: ${entry.lambda.status})`);
      }
      const source = typeof req.body?.source === 'string' ? req.body.source : '';
      if (!source.trim()) {
        return fail(res, 400, 'source must be a non-empty string');
      }
      try {
        await deploySource(client, entry, source);
      } catch (err) {
        // Almost always a failed in-guest syntax check — surface the
        // compiler output as a 400; the last-good handler stays live.
        return fail(res, 400, err instanceof Error ? err.message : String(err));
      }
      console.log(`[webapp] lambda ${entry.lambda.slug} redeployed (VM ${entry.vmId})`);
      res.json({ ok: true });
    }),
  );

  // The public invoke URL. Unauthenticated by design (like an AWS function
  // URL with auth-type NONE): the webapp holds the hostd token and mints a
  // short-lived proxy JWT per call.
  router.all(
    '/f/:slug',
    handle(async (req, res) => {
      const found = registry.lambdaBySlug(req.params.slug);
      if (!found || !found.vm.lambda) {
        return fail(res, 404, `no lambda named "${req.params.slug}"`);
      }
      // Body handling: only JSON content-types were consumed by the
      // app-level express.json (it still sets req.body = {} when the
      // content-type doesn't match, so key on the header, not req.body);
      // anything else is still on the wire — collect it raw.
      const ct = String(req.headers['content-type'] ?? '');
      let body: string;
      if (ct.includes('application/json') && req.body !== undefined) {
        body = typeof req.body === 'string' ? req.body : JSON.stringify(req.body);
      } else {
        const chunks: Buffer[] = [];
        for await (const chunk of req) {
          chunks.push(chunk as Buffer);
        }
        body = Buffer.concat(chunks).toString('utf8');
      }
      const q = req.url.includes('?') ? req.url.slice(req.url.indexOf('?')) : '';
      const result = await invokeLambda(cfg, client, found.project, found.vm, {
        method: req.method,
        queryString: q,
        body,
      });
      res
        .status(result.status)
        .set('content-type', result.contentType)
        .set('x-lambda-duration-ms', String(result.durationMs))
        .send(result.body);
    }),
  );

  return router;
}
