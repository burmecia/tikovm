# tikovm × tiko demo webapp

Single-page demo that drives tikovm (Firecracker microVM management) and the
tiko postgres guest image end to end. Three panels:

- **top** — vmtop-style live inventory of all demo VMs (1s poll)
- **left** — projects (create/delete), each with its nested VM list
- **right** — operations on the selected VM: a SQL console, a copyable psql
  connection string (mints a 1h proxy token) and **database branching** for
  the project's tiko postgres VM; exec-in-guest and delete for extra VMs
  (the tiko VM goes away with its project; it auto-suspends when idle and
  wakes on the next request); for **lambda** VMs, a source editor with
  save & deploy, a copyable public invoke URL and a test-invoke button

## Lambda functions

Any ready project can host **lambda functions** (AWS Lambda–style): pick
`node-22 (λ lambda)` / `python-3.12 (λ lambda)` in the project's "+ VM"
form and give it a name — the name becomes the URL slug. (The node-22 and
python-3.12 images are only offered as lambdas; `ubuntu-24` is the only
plain extra VM.) A lambda is a
`permanent` VM running a tiny per-language HTTP runtime as a systemd
service on guest port 8080 (deliberately not a hostd workload: active
workloads block hostd's auto-suspend gate). The Postgres drivers the
default handler needs (`pg` / `psycopg2`) are baked into the guest images
at build time, so deploying is boot + a few file writes.

- **invoke**: `ANY /api/demo/f/<slug>` — public (auth-type NONE, like an
  AWS function URL); the webapp mints a 60s hostd proxy JWT per call and
  forwards method/query/body to the guest runtime, passing the response
  status/content-type/body back verbatim (`x-lambda-duration-ms` header
  included). The handler receives `{method, path, body}` and may return a
  string, a JSON-able value, or `{statusCode, headers, body}`.
- **edit**: the right panel's textarea edits the handler source; "save &
  deploy" syntax-checks it in the guest (`node --check` / `py_compile`)
  via a temp file, so a broken edit never replaces the last-good handler.
  The runtime reloads the source per request — no restart needed.
- **auto-suspend**: after 120s without a request hostd snapshots the VM
  (zero CPU/memory); the next invoke transparently restores it (~1s cold
  start — snapshot restore, not a boot, so the runtime process resumes).
  Invoking a lambda also wakes the project's tiko postgres VM when it is
  suspended (it has its own auto-suspend), so the default handler's
  database read works on a cold start.
- **default handler**: reads `now(), current_database(), current_user`
  from the project's tiko postgres (`TIKO_DB_HOST`/`TIKO_DB_NAME`/
  `TIKO_DB_USER` env, written at provision; same subnet, pg_hba-trusted,
  no password).

## PostgREST database APIs

Pick `postgrest (REST API)` in a ready project's "+ VM" form to get a full
REST API over the project's tiko postgres — no code required. The VM runs
the baked-in PostgREST binary (systemd, port 3000) pointed at the project's
database (`public` schema, anonymous role = postgres, demo-only trust).

- **request**: `ANY /api/demo/pgrst/<slug>/<table>[?filters]` — public, same
  model as lambda invokes. PostgREST semantics apply end to end: filters
  (`?done=eq.false&select=id,task`), inserts/updates with JSON bodies,
  `Prefer: return=representation|count=exact` (forwarded, `content-range`
  returned), pagination. The right panel's "copy curl command" copies a
  ready-to-run `curl <base>/<table>`.
- **auto-suspend**: same as lambdas — 120s idle → snapshot; the next
  request restores the VM (~1s) and wakes the project's tiko postgres VM
  first if it is suspended too.
- **schema changes**: PostgREST loads its schema cache at startup, so a
  table/column created after the VM booted is invisible at first. The proxy
  detects the stale-cache errors (PGRST204/PGRST205), asks PostgREST to
  reload (SIGUSR1), and retries transparently — the first request after a
  schema change just takes a couple of seconds longer.

## What a "project" is

Creating a project automatically provisions one **tiko postgres VM**: boot →
wait for the S3 Files mount → rewrite the guest's `tiko.env` with the
project's identity (`TIKO_DB_ID`/`TIKO_PROJECT_ID`/`TIKO_VM_ID`, unique per
project) → initialize the database with `tiko_branch restore`, branching
copy-on-write from the seed pack (`/mnt/s3files/tiko_backup/0.tar.zst`,
db_id=0 — every project db is a branch of the seed) → start postgres via the
image's `start_pg.sh`. The project turns `ready` with the database running, so
the SQL panel works immediately. The tiko VM is created with **auto-suspend**:
after 120s without client connections hostd snapshots it (the Firecracker
process stops, consuming no CPU/memory) and the next SQL/exec request
transparently restores it. Extra VMs (`ubuntu-24`), lambda functions
(node-22 / python-3.12) and PostgREST database APIs can be added alongside
(see below).

### Database branching

The "create branch" action on a tiko postgres VM creates a **new project**
whose database is a copy-on-write branch of the selected one:

1. the source VM is woken if suspended (any exec does this transparently) and
   its postgres is sanity-checked with `select 1`
2. `tiko_branch backup` packs the running database (pg_basebackup → tar.zst)
   to `/mnt/s3files/tiko_backup/branch-<dbId>.tar.zst` — the shared S3 Files
   mount is the only path both VMs can see, since source and branch live in
   different projects/subnets
3. a new project + tiko postgres VM is provisioned exactly like a plain one,
   except `tiko_branch restore` reads that pack with
   `--parent-db-id <source dbId>` instead of the seed
4. postgres is started, verified with `select 1`, and the pack file is
   deleted (best-effort also on failure)

Branches are copy-on-write over the shared tiko storage root and keep reading
their ancestors' chunks (transitively — branching a branch works), so
**deleting a project cascades to all its descendant branches**, whether the
deletion is manual, by TTL expiry, or at shutdown. The project list shows the
lineage (`⤷ branch of project #…`). The branch appears as a `provisioning`
project and follows the same status/step polling as project creation.

Projects expire after a TTL (default **1 hour**) and are deleted with all
their VMs. On shutdown (Ctrl-C) the webapp deletes every project, extra VM
and any leftover demo VM from previous runs (matched by the `tikovm-demo`
tag); hostd tears down the per-project bridges with the last VM.

## Layout

- `server/` — Express backend (holds the hostd token; the browser never sees
  it). Serves the SPA and the `/api/demo` REST surface. Reuses the official
  Node client (`tikovm`, `file:../../clients/node`) for all hostd traffic.
  `lambda.ts` owns the lambda runtime sources, provisioning, deploy and
  invoke path; `postgrest.ts` the PostgREST provisioning and REST proxy.
- `web/` — React + Vite frontend (3 panels, polling `/api/demo/overview`).

## Setup & run

```bash
# one-time: build the tikovm client lib, then install workspaces
cd webapp && npm run setup

# build + run the server (serves the built SPA on :4000)
npm run build
TIKOVM_HOSTD_API_TOKEN=xxx npm start

# or develop: backend with live reload + Vite dev server (proxies /api)
npm run dev:server   # terminal 1
npm run dev:web      # terminal 2 → http://localhost:5173
```

hostd must already be running (see `scripts/run_hostd.sh`).

### Environment

| var | default | meaning |
|---|---|---|
| `PORT` | `4000` | webapp listen port |
| `HOSTD_URL` | `http://127.0.0.1:3000` | hostd API base URL |
| `TIKOVM_HOSTD_API_TOKEN` | — (required) | hostd Bearer token |
| `PROJECT_TTL_MS` | `3600000` | project lifetime (e.g. `120000` to demo expiry) |
| `DEMO_TAG` | `tikovm-demo` | tag marking the app's VMs (orphan sweep) |
| `TIKO_ORG_ID` | `12` | tiko org id baked into every `tiko.env` |
| `PROXY_HOST` | auto-detected EC2 public IPv4 (fallback: `HOSTD_URL` hostname) | host written into the psql connection string |
| `PROXY_PORT` | `8080` | hostd proxy listener port (for the connection string) |

## Tests

```bash
npm test          # server unit tests (registry ids/expiry, lambda slugs, tiko.env builder)
npm run typecheck # tsc --noEmit for server + web
```
