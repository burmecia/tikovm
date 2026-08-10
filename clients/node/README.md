# tikovm — Node.js client for hostd

A TypeScript client library for the [tikovm](https://github.com/anomalyco/tikovm)
`hostd` HTTP API, giving you a typed, promise-based interface to manage
Firecracker microVMs. Covers the **VM lifecycle** plus the per-VM
**network** and **exposed-port** endpoints; the workloads/proxy endpoint
follow in a later iteration.

## Install

```bash
npm install tikovm
```

Requires Node >= 18 (uses the built-in `fetch`; zero runtime dependencies).

## Quickstart

```ts
import Tikovm from 'tikovm';

const client = new Tikovm({
  accessToken: process.env.TIKOVM_HOSTD_API_TOKEN!, // required by hostd
  baseUrl: 'http://localhost:3000', // optional, defaults to this
});

// Create and start a VM. hostd-required fields you omit (mode, cpus,
// memory_mb, disk_size_mb, network_config, ssh_access) get defaults.
const vm = await client.vms.create({
  name: 'my vm',
  project_id: 123,
  image: 'ubuntu-24',
  cpus: 2,
  memory_mb: 1024,
  network_config: { allow_internet: true },
});

console.log(vm.id);            // e.g. vm-123-abc123
await vm.refresh();            // fetch live state
console.log(vm.state);         // 'started' | 'paused' | 'suspended' | ...
console.log(vm.net?.guest_ip); // allocated guest IP

// Lifecycle — each call updates the wrapper's cached state.
await vm.pause();
await vm.resume();
await vm.snapshot();           // suspends the VM
await vm.restore();

// Network config (read-only) and exposed-port registry.
const network = await vm.network.get();          // GET  /api/vms/{id}/network
await vm.ports.expose({ port: 8080, label: 'web' }); // POST /api/vms/{id}/ports
const ports = await vm.ports.list();             // GET  /api/vms/{id}/ports
const jwt = await vm.ports.token(8080, { proto: 'tcp', ttl_secs: 60 });
                                                 // POST /api/vms/{id}/ports/{port}/token
await vm.ports.remove(8080);                     // DELETE /api/vms/{id}/ports/{port}

// Run a command inside the guest and block until it exits.
const result = await vm.exec(['echo', 'hello']);
console.log(result.exit_code, result.logs);

// Destroy the VM.
await vm.delete();

// Or drive lifecycle without a wrapper:
const all = await client.vms.list();
await client.vms.pause(all[0].id);
```

## API

### `new Tikovm({ accessToken, baseUrl? })`

- `accessToken` — Bearer token hostd checks on every request (it refuses to
  start without a non-empty `TIKOVM_HOSTD_API_TOKEN`).
- `baseUrl` — hostd's API listener (the one serving `/api`). Default
  `http://localhost:3000`. Do not include a trailing `/api` or slash.

### `client.vms`

| Method | Endpoint |
| --- | --- |
| `list(): Promise<Vm[]>` | `GET /api/vms` |
| `get(id): Promise<Vm>` | `GET /api/vms/{id}` |
| `create(config): Promise<Vm>` | `POST /api/vms` |
| `delete(id): Promise<void>` | `DELETE /api/vms/{id}` |
| `pause(id) / resume(id) / restore(id): Promise<Vm>` | `POST /api/vms/{id}/{op}` |
| `snapshot(id): Promise<VmSnapshot>` | `POST /api/vms/{id}/snapshot` |

`create` accepts a `VmCreateConfig` (`name`, `project_id`, `image` required;
everything else optional). Omitted fields are filled with defaults matching
hostd's `VmConfig` requirements: `mode: 'ephemeral'`, `cpus: 1`,
`memory_mb: 512`, `disk_size_mb: 1024`, `network_config:
{ allow_internet: false, exposed_ports: [], egress: [], public_access: false }`,
`ssh_access: false`, empty `env`/`cmd`/`services`/`tags`, and `null` for
`cron_schedule`/`timeout_secs`/`auto_suspend`/`block_storage`.

### `Vm` wrapper

Returned by the `client.vms` methods; bound to a `vm_id` and caches the last
`VmInstance` from the API.

- Accessors: `id`, `state`, `vmConfig`, `net`, plus predicates `isRunning`,
  `isPaused`, `isSuspended`, `isDestroyed`.
- Lifecycle methods: `refresh()`, `pause()`, `resume()`, `snapshot()`,
  `restore()`, `delete()`, `exec(cmd, { env?, cwd? }?)`. `exec` is hostd's
  synchronous `/exec` wrapper: it runs a command in the guest, waits for it
  to finish, and returns the `Workload` flattened with its captured `logs`
  (`WorkloadLogEntry[]`).
- `network` namespace (`vm.network.get()`): read-only network config.
- `ports` namespace (`vm.ports.*`): exposed-port registry and proxy-token
  minting.

For id-only callers, `client.vms.network(id)` and `client.vms.ports(id)`
return the same namespaces without going through a `Vm` wrapper.

### `vm.network` and `vm.ports`

| Method | Endpoint |
| --- | --- |
| `vm.network.get(): Promise<NetworkConfig>` | `GET /api/vms/{id}/network` |
| `vm.ports.list(): Promise<ExposedPort[]>` | `GET /api/vms/{id}/ports` |
| `vm.ports.expose({ port, label }): Promise<ExposedPort>` | `POST /api/vms/{id}/ports` |
| `vm.ports.remove(port): Promise<void>` | `DELETE /api/vms/{id}/ports/{port}` |
| `vm.ports.token(port, { ttl_secs?, proto? }?): Promise<PortToken>` | `POST /api/vms/{id}/ports/{port}/token` |

`proto` is `'http'` (default) or `'tcp'` (e.g. the Postgres wire protocol).
Minting a token requires the port to be currently exposed, and unexposing a
port revokes proxy access immediately (hostd re-validates the registry on
every connection).

### Errors

Every method rejects with a `TikovmError` subclass on failure:

- `TikovmApiError` — hostd responded non-2xx. `.status` is the HTTP status,
  `.code`/`.message` come from the uniform `{ "error": { code, message } }`
  body (see `hostd/src/api/error.rs`).
- `TikovmRequestError` — transport-level failure (e.g. connection refused).
- `TikovmProtocolError` — the response was not valid JSON.

## Development

```bash
npm install
npm run typecheck  # tsc --noEmit
npm test           # typechecks src+test, runs unit tests against a mock hostd
npm run build      # emits ESM + .d.ts into dist/
npm run build:e2e  # compiles src + test-e2e into dist-e2e/ for the live-hostd test
```

Unit tests run against an in-process mock HTTP server, so no root/KVM/hostd is
needed. The wire types in `src/types.ts` mirror hostd's serde JSON shapes
(`hostd/src/vmm/vm.rs`, `hostd/src/vmm/workload.rs`, `hostd/src/net/types.rs`).

There is also a live end-to-end test (`test-e2e/e2e.test.ts`) that drives the
whole VM lifecycle through the client against a real hostd. It needs
root/KVM/a Firecracker binary and is run by the repo's bash suite:
`tests/test_node_client.sh` (or `tests/run_all.sh`). It expects
`TIKOVM_HOSTD_URL` and `TIKOVM_HOSTD_TOKEN` in the environment (the wrapper
sets both from `tests/common.sh`).
