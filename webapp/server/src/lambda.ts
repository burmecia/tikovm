//! Lambda functions (AWS Lambda–style) on top of plain tikovm VMs.
//!
//! A lambda VM is a `permanent` node-22/python-3.12 VM running a tiny
//! per-language HTTP runtime as a **systemd service** (deliberately not a
//! hostd workload: active workloads block hostd's auto-suspend gate) on
//! guest port 8080. Invocations arrive through hostd's HTTP proxy:
//!
//!   caller → webapp `ALL /api/demo/f/<slug>` → mint 60s proxy JWT →
//!   hostd proxy → guest :8080
//!
//! Auto-suspend needs no extra wiring: the VM exposes port 8080 and sets
//! `auto_suspend.idle_timeout_secs`, so hostd's HTTP idle detector
//! snapshots it after a quiet period and the proxy transparently restores
//! it on the next invoke (snapshot/restore freezes guest memory, so the
//! runtime server resumes with the VM — the caller just sees a slow first
//! request).
//!
//! The runtime wrapper reloads the handler source on every request, so
//! saving new code takes effect with no restart. Saves go through a
//! syntax-checked temp file, so a broken edit never replaces the last-good
//! handler. The Postgres drivers the default handler needs (`pg` /
//! `psycopg2`) are baked into the guest images at build time (see
//! scripts/rootfs/build_rootfs_{node22,python312}.sh) — provisioning is
//! boot + a few file writes.

import type { Tikovm } from 'tikovm';
import type { Config } from './config.js';
import {
  execOk,
  execUntil,
  rawGetVm,
  waitForState,
} from './hostd.js';
import { psqlArgv } from './provision.js';
import type {
  LambdaLanguage,
  Project,
  ProjectVmEntry,
} from './state.js';

/** Guest port the lambda runtime listens on; exposed at VM creation. */
export const LAMBDA_PORT = 8080;

/** Idle seconds without a proxied request before hostd snapshots the VM. */
export const LAMBDA_IDLE_TIMEOUT_SECS = 120;

/** Guest image per lambda language (drivers baked in at image build). */
export const LAMBDA_IMAGES: Record<LambdaLanguage, string> = {
  node: 'node-22',
  python: 'python-3.12',
};

const HANDLER_PATH: Record<LambdaLanguage, string> = {
  node: '/opt/lambda/handler.mjs',
  python: '/opt/lambda/handler.py',
};

// ── guest runtime wrappers (written verbatim into the VM) ─────────────────

const NODE_SERVER = `// tikovm lambda runtime (node): HTTP wrapper around
// /opt/lambda/handler.mjs. The handler module is re-imported per request
// (cache-busted query), so saving new source takes effect with no restart.
import http from 'node:http';
import { pathToFileURL } from 'node:url';

const HANDLER_URL = pathToFileURL('/opt/lambda/handler.mjs').href;

function readBody(req) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    req.on('data', (c) => chunks.push(c));
    req.on('end', () => resolve(Buffer.concat(chunks).toString('utf8')));
    req.on('error', reject);
  });
}

function send(res, status, headers, body) {
  res.writeHead(status, headers);
  res.end(body);
}

function sendResult(res, out) {
  if (typeof out === 'string') {
    send(res, 200, { 'content-type': 'text/plain; charset=utf-8' }, out);
  } else if (out && typeof out === 'object' && ('statusCode' in out || 'body' in out)) {
    // API-Gateway-style {statusCode, headers, body} passthrough.
    const body = typeof out.body === 'string' ? out.body : JSON.stringify(out.body ?? null);
    send(res, out.statusCode ?? 200, out.headers ?? { 'content-type': 'application/json' }, body);
  } else {
    send(res, 200, { 'content-type': 'application/json' }, JSON.stringify(out ?? null));
  }
}

http.createServer(async (req, res) => {
  try {
    if (req.method === 'GET' && req.url === '/__health') {
      send(res, 200, { 'content-type': 'text/plain' }, 'ok');
      return;
    }
    const body = await readBody(req);
    const mod = await import(HANDLER_URL + '?t=' + Date.now());
    if (typeof mod.handler !== 'function') {
      throw new Error('handler.mjs must export a handler(event) function');
    }
    const event = { method: req.method, path: req.url, body };
    sendResult(res, await mod.handler(event));
  } catch (err) {
    send(res, 500, { 'content-type': 'application/json' },
      JSON.stringify({ error: String((err && err.stack) || err) }));
  }
}).listen(8080, '0.0.0.0');
`;

const PYTHON_SERVER = `# tikovm lambda runtime (python): HTTP wrapper around
# /opt/lambda/handler.py. The handler file is exec'd fresh per request, so
# saving new source takes effect with no restart.
import json
import traceback
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HANDLER_PATH = '/opt/lambda/handler.py'


def load_handler():
    with open(HANDLER_PATH) as f:
        source = f.read()
    ns = {}
    exec(compile(source, HANDLER_PATH, 'exec'), ns)
    handler = ns.get('handler')
    if not callable(handler):
        raise RuntimeError('handler.py must define a handler(event) function')
    return handler


class LambdaRequestHandler(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def log_message(self, *args):
        pass

    def _send(self, status, content_type, body):
        data = body.encode('utf-8')
        self.send_response(status)
        self.send_header('content-type', content_type)
        self.send_header('content-length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def _send_result(self, out):
        if isinstance(out, str):
            self._send(200, 'text/plain; charset=utf-8', out)
        elif isinstance(out, dict) and ('statusCode' in out or 'body' in out):
            # API-Gateway-style {statusCode, headers, body} passthrough.
            body = out.get('body')
            if not isinstance(body, str):
                body = json.dumps(body)
            headers = out.get('headers') or {}
            self._send(out.get('statusCode', 200),
                       headers.get('content-type', 'application/json'), body)
        else:
            self._send(200, 'application/json', json.dumps(out))

    def _handle(self):
        try:
            if self.command == 'GET' and self.path == '/__health':
                self._send(200, 'text/plain', 'ok')
                return
            length = int(self.headers.get('content-length') or 0)
            body = self.rfile.read(length).decode('utf-8') if length else ''
            event = {'method': self.command, 'path': self.path, 'body': body}
            self._send_result(load_handler()(event))
        except Exception:
            self._send(500, 'application/json',
                       json.dumps({'error': traceback.format_exc()}))

    do_GET = _handle
    do_POST = _handle
    do_PUT = _handle
    do_PATCH = _handle
    do_DELETE = _handle


ThreadingHTTPServer(('0.0.0.0', 8080), LambdaRequestHandler).serve_forever()
`;

const SERVER_SOURCE: Record<LambdaLanguage, { path: string; content: string }> = {
  node: { path: '/opt/lambda/server.mjs', content: NODE_SERVER },
  python: { path: '/opt/lambda/server.py', content: PYTHON_SERVER },
};

// ── default handlers (database-read example against the project's tiko pg) ─

const NODE_DEFAULT_HANDLER = `// Default lambda: read from the project's tiko postgres.
// TIKO_DB_HOST / TIKO_DB_NAME / TIKO_DB_USER come from /opt/lambda/lambda.env.
import pg from 'pg';

export async function handler(event) {
  const client = new pg.Client({
    host: process.env.TIKO_DB_HOST,
    database: process.env.TIKO_DB_NAME ?? 'postgres',
    user: process.env.TIKO_DB_USER ?? 'postgres',
  });
  await client.connect();
  try {
    const { rows } = await client.query(
      'select now() as db_time, current_database() as database, current_user as user',
    );
    return { event: { method: event.method, path: event.path }, rows };
  } finally {
    await client.end();
  }
}
`;

const PYTHON_DEFAULT_HANDLER = `# Default lambda: read from the project's tiko postgres.
# TIKO_DB_HOST / TIKO_DB_NAME / TIKO_DB_USER come from /opt/lambda/lambda.env.
import os

import psycopg2


def handler(event):
    conn = psycopg2.connect(
        host=os.environ['TIKO_DB_HOST'],
        dbname=os.environ.get('TIKO_DB_NAME', 'postgres'),
        user=os.environ.get('TIKO_DB_USER', 'postgres'),
    )
    try:
        with conn.cursor() as cur:
            cur.execute('select now(), current_database(), current_user')
            db_time, database, user = cur.fetchone()
        return {
            'event': {'method': event['method'], 'path': event['path']},
            'rows': [{'db_time': str(db_time), 'database': database, 'user': user}],
        }
    finally:
        conn.close()
`;

export const DEFAULT_HANDLER: Record<LambdaLanguage, string> = {
  node: NODE_DEFAULT_HANDLER,
  python: PYTHON_DEFAULT_HANDLER,
};

/** systemd unit for the runtime service; ExecStart is language-specific. */
function lambdaUnit(language: LambdaLanguage): string {
  const execStart =
    language === 'node'
      ? '/usr/local/bin/node /opt/lambda/server.mjs'
      : '/usr/bin/python3 /opt/lambda/server.py';
  return `[Unit]
Description=tikovm lambda runtime
After=network.target

[Service]
WorkingDirectory=/opt/lambda
EnvironmentFile=-/opt/lambda/lambda.env
ExecStart=${execStart}
Restart=always
RestartSec=1

[Install]
WantedBy=multi-user.target
`;
}

// ── guest file helpers ────────────────────────────────────────────────────

/** Write a file in the guest via base64 (no shell-quoting pitfalls). */
async function writeGuestFile(
  client: Tikovm,
  vmId: string,
  path: string,
  content: string,
): Promise<void> {
  const b64 = Buffer.from(content, 'utf8').toString('base64');
  const dir = path.slice(0, path.lastIndexOf('/'));
  await execOk(
    client,
    vmId,
    ['bash', '-c', `mkdir -p '${dir}' && printf %s '${b64}' | base64 -d > '${path}'`],
    `write ${path}`,
  );
}

// ── provision ─────────────────────────────────────────────────────────────

/**
 * Provision a lambda VM asynchronously; updates `entry.lambda`
 * status/step/error. The entry is pushed into `project.vms` with a
 * placeholder vmId so the UI sees the deploy immediately; the real id is
 * filled in as soon as hostd answers the create call.
 *
 * On failure the lambda is left in `error` with its (possibly
 * half-provisioned) VM for debugging, same policy as project provisioning.
 */
export async function provisionLambda(
  cfg: Config,
  client: Tikovm,
  project: Project,
  entry: ProjectVmEntry,
): Promise<void> {
  const meta = entry.lambda!;
  const language = meta.language;
  const setStep = (step: string) => {
    meta.step = step;
  };
  try {
    setStep('creating the VM');
    const vm = await client.vms.create({
      name: entry.name,
      project_id: project.id,
      image: LAMBDA_IMAGES[language],
      // permanent (not ephemeral) so hostd can auto-suspend it.
      mode: 'permanent',
      cpus: 1,
      memory_mb: 256,
      disk_size_mb: 1024,
      network_config: {
        allow_internet: true,
        // The runtime's HTTP port; the proxy only forwards to exposed ports.
        exposed_ports: [{ port: LAMBDA_PORT, label: 'lambda' }],
      },
      // Snapshot after 2 min without a proxied request (HTTP-activity
      // detector; no idle_check_cmd — every invoke passes the proxy). The
      // next invoke transparently restores the VM.
      auto_suspend: { idle_timeout_secs: LAMBDA_IDLE_TIMEOUT_SECS },
      tags: [cfg.demoTag],
    });
    entry.vmId = vm.id;

    setStep('waiting for the VM to boot');
    await waitForState(client, vm.id, 'started', 180_000);

    // The default handler connects to the project's tiko postgres. Same
    // project = same subnet, and the image's pg_hba trusts that subnet (the
    // rule hostd seeds at VM creation), so no password is needed. The IP is
    // host-side state, readable even while the tiko VM is suspended.
    setStep('resolving the tiko postgres address');
    const tikoVm = project.vms.find((v) => v.kind === 'tiko');
    if (!tikoVm) {
      throw new Error(`project ${project.id} has no tiko postgres VM`);
    }
    const tiko = await rawGetVm(cfg, tikoVm.vmId);
    const dbHost = tiko.net?.guest_ip;
    if (!dbHost) {
      throw new Error('the tiko postgres VM has no guest IP yet');
    }

    setStep('installing the lambda runtime');
    await writeGuestFile(client, vm.id, SERVER_SOURCE[language].path, SERVER_SOURCE[language].content);
    await writeGuestFile(client, vm.id, HANDLER_PATH[language], meta.source);
    await writeGuestFile(client, vm.id, '/opt/lambda/lambda.env',
      `TIKO_DB_HOST=${dbHost}\nTIKO_DB_NAME=postgres\nTIKO_DB_USER=postgres\n`);
    await writeGuestFile(client, vm.id, '/etc/systemd/system/tikovm-lambda.service',
      lambdaUnit(language));
    await execOk(client, vm.id,
      ['systemctl', 'enable', '--now', 'tikovm-lambda.service'],
      'lambda service start');

    setStep('waiting for the runtime');
    await execUntil(
      client,
      vm.id,
      ['curl', '-fsS', `http://127.0.0.1:${LAMBDA_PORT}/__health`],
      'the lambda runtime',
      60_000,
    );

    meta.status = 'ready';
    meta.step = '';
    console.log(
      `[webapp] lambda ${meta.slug} (${language}) ready on VM ${vm.id} ` +
        `(project ${project.id})`,
    );
  } catch (err) {
    meta.status = 'error';
    meta.step = '';
    meta.error = err instanceof Error ? err.message : String(err);
    console.error(`[webapp] lambda ${meta.slug} deploy failed: ${meta.error}`);
    // If the VM was never created there is nothing to debug — drop the
    // placeholder entry entirely.
    if (!entry.vmId) {
      project.vms = project.vms.filter((v) => v !== entry);
    }
  }
}

// ── deploy (save new source) ──────────────────────────────────────────────

/**
 * Deploy new handler source: write to a temp file, syntax-check it with the
 * guest's own toolchain, then move it into place — a broken edit never
 * kills the last-good handler. The runtime reloads the source per request,
 * so the new code is live as soon as this returns. Throws (→ 400) with the
 * compiler output when the check fails.
 */
export async function deploySource(
  client: Tikovm,
  entry: ProjectVmEntry,
  source: string,
): Promise<void> {
  const meta = entry.lambda!;
  const handler = HANDLER_PATH[meta.language];
  const tmp = handler.replace(/handler\./, 'handler.check.');
  await writeGuestFile(client, entry.vmId, tmp, source);
  const check =
    meta.language === 'node'
      ? ['/usr/local/bin/node', '--check', tmp]
      : ['python3', '-m', 'py_compile', tmp];
  await execOk(client, entry.vmId, check, 'syntax check');
  await execOk(client, entry.vmId, ['mv', tmp, handler], 'activate handler');
  meta.source = source;
}

// ── invoke ────────────────────────────────────────────────────────────────

export interface InvokeResult {
  status: number;
  contentType: string;
  body: string;
  durationMs: number;
}

/**
 * Invoke a lambda: mint a short-lived proxy JWT and forward the request to
 * hostd's HTTP proxy, which revalidates the target (waking the VM from
 * auto-suspend first if needed) and relays to the guest runtime. The
 * upstream status, content-type, and body pass through verbatim.
 *
 * The project's tiko postgres VM has its own auto-suspend, and nothing
 * guest-side can wake it (hostd owns that) — so when it is suspended, wake
 * and verify it (`select 1`, the same pattern as branch provisioning)
 * before forwarding, or a woken lambda would find its database
 * EHOSTUNREACH.
 */
export async function invokeLambda(
  cfg: Config,
  client: Tikovm,
  project: Project,
  entry: ProjectVmEntry,
  req: { method: string; queryString: string; body: string },
): Promise<InvokeResult> {
  const meta = entry.lambda!;
  if (meta.status !== 'ready') {
    throw new LambdaNotReady(
      `lambda ${meta.slug} is not ready (status: ${meta.status})`,
    );
  }
  const tikoVm = project.vms.find((v) => v.kind === 'tiko');
  if (tikoVm) {
    const tiko = await rawGetVm(cfg, tikoVm.vmId);
    if (tiko.state !== 'started') {
      await execOk(client, tikoVm.vmId, psqlArgv('select 1'), 'wake tiko postgres');
    }
  }
  const { token } = await client.vms
    .ports(entry.vmId)
    .token(LAMBDA_PORT, { proto: 'http', ttl_secs: 60 });

  // hostd's proxy listener is on the same host as its API (and this app).
  const proxyBase = `http://${new URL(cfg.hostdUrl).hostname}:${cfg.proxyPort}`;
  const started = Date.now();
  const canHaveBody = req.method !== 'GET' && req.method !== 'HEAD';
  const upstream = await fetch(`${proxyBase}/${req.queryString}`, {
    method: req.method,
    headers: { authorization: `Bearer ${token}` },
    body: canHaveBody && req.body ? req.body : undefined,
    signal: AbortSignal.timeout(30_000),
  });
  return {
    status: upstream.status,
    contentType: upstream.headers.get('content-type') ?? 'text/plain',
    body: await upstream.text(),
    durationMs: Date.now() - started,
  };
}

/** Thrown when invoking a lambda that is still deploying / in error. */
export class LambdaNotReady extends Error {}
