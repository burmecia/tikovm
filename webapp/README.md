# tikovm × tiko demo webapp

Single-page demo that drives tikovm (Firecracker microVM management) and the
tiko postgres guest image end to end. Three panels:

- **top** — vmtop-style live inventory of all demo VMs (1s poll)
- **left** — projects (create/delete), each with its nested VM list
- **right** — operations on the selected VM: lifecycle (pause / resume /
  snapshot / restore / delete), exec-in-guest, and a SQL console for the
  project's tiko postgres VM

## What a "project" is

Creating a project automatically provisions one **tiko postgres VM**: boot →
wait for the S3 Files mount → rewrite the guest's `tiko.env` with the
project's identity (`TIKO_DB_ID`/`TIKO_PROJECT_ID`/`TIKO_VM_ID`, unique per
project). Database initialization will use a **backup/restore** flow (the
image's `init_pg.sh` path is intentionally unused) — not implemented yet, so
the project turns `ready` with the identity in place but no running database
(the SQL panel will fail until then). Extra VMs (`ubuntu-24` / `python-3.12`
/ `node-22`) can be added alongside.

Projects expire after a TTL (default **1 hour**) and are deleted with all
their VMs. On shutdown (Ctrl-C) the webapp deletes every project, extra VM
and any leftover demo VM from previous runs (matched by the `tikovm-demo`
tag); hostd tears down the per-project bridges with the last VM.

## Layout

- `server/` — Express backend (holds the hostd token; the browser never sees
  it). Serves the SPA and the `/api/demo` REST surface. Reuses the official
  Node client (`tikovm`, `file:../../clients/node`) for all hostd traffic.
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

## Tests

```bash
npm test          # server unit tests (registry ids/expiry, tiko.env builder)
npm run typecheck # tsc --noEmit for server + web
```
