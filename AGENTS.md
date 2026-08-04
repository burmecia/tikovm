# AGENTS.md

Guidance for AI coding agents working in this repository. Assumes no prior
knowledge of the project.

## Project overview

`tikovm` is a Firecracker microVM management system written in Rust. It is a
Cargo workspace with two binary crates:

- **`hostd/`** — the host daemon. An async (tokio/axum) HTTP API server that
  manages the full lifecycle of Firecracker microVMs (create, start, pause,
  resume, snapshot, restore, destroy), per-project host networking (bridges,
  TAP devices, iptables NAT), and "workloads" (processes executed inside a
  guest). It drives each VM's Firecracker process over its API Unix socket
  and talks to the guest agent over vsock.
- **`guestd/`** — the guest agent. A small, dependency-light binary that runs
  inside every VM (installed as a systemd service in the guest image). It
  listens on vsock port 5000 and executes workloads on hostd's request,
  streaming stdout/stderr and exit status back as newline-delimited JSON
  events (protocol defined in `guestd/src/proto.rs`).

The two crates are separate binaries deployed to different sides of the VM
boundary; guestd deliberately avoids tokio/axum so it stays small for the
guest image.

## Repository layout

```
Cargo.toml            workspace manifest (members: hostd, guestd); all deps
                      are pinned as workspace dependencies here
hostd/
  src/main.rs         CLI args (clap), wiring: NetworkManager -> FirecrackerVmm -> ApiServer
  src/error.rs        crate-wide Error/Result (thiserror)
  src/api/            axum HTTP API: server.rs (router + Bearer auth middleware),
                      error.rs (uniform JSON errors),
                      routes/{health,vm,network,ports,workload}.rs
  src/net/            host networking: cidr.rs (IPv4 CIDR math), state.rs (pure
                      IPAM allocator, unit-tested), host.rs (`ip`/`iptables`
                      shell-outs), manager.rs (NetworkManager: ties state to
                      host effects, persists network_state.json), types.rs
  src/vmm/            Vmm trait (mod.rs), vm.rs (VmConfig/VmInstance/VmState),
                      workload.rs, firecracker/ (the only Vmm implementation):
                      vmm.rs (FirecrackerVmm), setup.rs (spawn + pre-boot API
                      config), api.rs (Firecracker API socket client),
                      vsock.rs (channel to guestd), guest.rs
guestd/
  src/main.rs         vsock accept loop, one thread per host connection
  src/proto.rs        newline-delimited JSON control protocol (both directions)
  src/agent.rs        workload spawn/stop/track, output forwarding
  src/connection.rs   per-connection request dispatch
  src/vsock.rs        vsock listener/stream via libc
scripts/              project-wide bash: run_hostd.sh,
                      download_kernel.sh, build_rootfs_ubuntu24.sh,
                      build_initramfs.sh, initramfs_init.sh
tests/                end-to-end tests: common.sh (shared helpers), run_all.sh
                      (full suite), test_{vm_lifecycle,workloads,exec,
                      pause_resume,snapshot_restore,networking,ports}.sh
                      (each self-contained)
assets/               VM boot artifacts: vmlinux kernel, ubuntu-24.04-rootfs.ext4,
                      initramfs.cpio.gz (some are build artifacts; .gitkeep'd)
```

## Technology stack and runtime architecture

- Rust, edition 2024, `rust-version = 1.96`; tokio ("full"), axum 0.8,
  tower-http, serde/serde_json, clap (derive), tracing, thiserror.
- Firecracker is an **external binary**, located via the `FIRECRACKER_BIN`
  env var or found on PATH (`vmm/firecracker/setup.rs` `from_path_or_env`).
- Boot chain per VM: Firecracker boots `assets/vmlinux-*.bin` with
  `assets/initramfs.cpio.gz`; the initramfs `/init`
  (`scripts/initramfs_init.sh`) assembles an overlayfs root —
  `/dev/vda` = shared read-only Ubuntu base image (lower), `/dev/vdb` =
  per-VM writable overlay disk (upper+work) — then switch_roots into
  systemd. Every VM shares the immutable base image while keeping its own
  writable state.
- Networking (see README.md): each project gets a bridge `tbr-<project_id>`
  with a subnet carved from `--net-supernet` (default `172.16.0.0/12`) at
  `--net-subnet-prefix` (default /24). VMs in one project share the subnet;
  the host bridge IP `.1` is the gateway; egress is per-subnet iptables
  MASQUERADE. Bridge/subnet is created with the project's first VM and torn
  down with its last; allocation state persists to
  `<work_dir>/network_state.json` and is reconciled on startup. The guest IP
  is passed as a kernel `ip=` boot argument (`CONFIG_IP_PNP=y`), so eth0 is
  configured before init runs.
- Workloads: hostd connects to guestd via the VM's Firecracker vsock UDS
  (`CONNECT 5000`) and exchanges newline-delimited JSON (`guestd/src/proto.rs`
  documents the message shapes at the top of the file). Guest stdout/stderr
  is forwarded to the host and queryable through the API.
- Runtime state lives under `--work-dir` (default `/tmp/tikovm`):
  `<work_dir>/<vm_id>/` holds the Firecracker API socket (`<vm_id>.socket`),
  vsock UDS, serial console log (`<vm_id>.serial.log`), overlay disk, and
  snapshot files. Snapshotting a VM stops its Firecracker process; only the
  snapshot files remain until restore.
- API: mounted under `/api` on `--api-listen` (default `0.0.0.0.0:3000`).
  Endpoints include `/api/health`, `/api/vms` (CRUD plus
  `/{id}/pause|resume|snapshot|restore|exec`, nested `/{id}/network`,
  `/{id}/ports` and `/{id}/workloads`). `/{id}/exec` is a synchronous wrapper
  over the workloads API: it starts a workload, polls for the terminal state,
  and returns the workload plus its captured logs in one response.
  `/{id}/ports` manages the VM's exposed ports (`{port, label}` for HTTP
  workloads, stored in `NetworkConfig.exposed_ports`, an initial set is
  accepted in the create-VM body); it is a VM-side registry only — no host
  port forwarding exists yet (a JWT reverse proxy is planned). All
  failures return a uniform JSON error body
  `{"error": {"code": <http status>, "message": ...}}` (see
  `hostd/src/api/error.rs`).

## Build, run, and test commands

```bash
cargo build -p hostd                 # build the host daemon
cargo build -p guestd                # build the guest agent
cargo test                           # unit tests (hostd/src/net/state.rs, cidr.rs)
cargo clippy                         # lints are kept clean (see git history)
scripts/run_hostd.sh                   # build as current user, run via sudo -E
tests/run_all.sh                       # full end-to-end suite (see below)
```

Runtime requirements and environment:

- **hostd must run as root** — it creates bridges/TAPs, iptables NAT rules,
  and loop-mounts overlay disks. `run_hostd.sh` builds as the current user
  (so `target/` stays user-writable) and execs the binary via `sudo -E`.
- `TIKOVM_HOSTD_API_TOKEN` (required, non-empty): Bearer token the API
  middleware checks on every request; hostd refuses to start without it.
- `FIRECRACKER_BIN`: path to the firecracker binary (the scripts default to
  `$HOME/firecracker/build/cargo_target/x86_64-unknown-linux-musl/debug/firecracker`).
- `RUST_LOG`: tracing env-filter, e.g. `RUST_LOG=hostd=debug,tower_http=debug`.
- Only Linux/x86_64 is supported (Firecracker, vsock, bridges, overlayfs).

There are no Rust integration tests and no CI configuration. Testing is:

1. **Unit tests** — `cargo test`; coverage exists for the pure networking
   logic (`net/state.rs` IPAM allocator, `net/cidr.rs`). Add tests in the
   same `#[cfg(test)] mod tests` style when touching pure logic.
2. **End-to-end** — `tests/run_all.sh` requires root/KVM and a
   Firecracker binary. It runs the self-contained `tests/test_*.sh` files
   (vm_lifecycle, pause_resume, snapshot_restore, workloads, exec,
   networking),
   each of which starts hostd via `run_hostd.sh` and exercises its slice of
   the API surface with curl/jq: VM create/get/list/delete, pause/
   resume, snapshot/restore, workload run/stop/logs, per-project bridge and
   TAP topology, guest ping from the host, and teardown of networking when
   the project's last VM is deleted. Shared scaffolding (hostd start/stop,
   API helpers, VM/boot/workload polling) lives in `tests/common.sh`. Run
   the suite (or a single `tests/test_*.sh` file) after changes to the API,
   VMM lifecycle, or networking. There is no Rust unit-test coverage for
   the VMM or API layers — the e2e script is the safety net there.

## Asset (guest image) build process

The boot artifacts in `assets/` are produced by scripts, not committed
blindly:

- `download_kernel.sh` — fetches the latest Firecracker CI kernel from S3
  and symlinks `vmlinux.bin`.
- `build_rootfs_ubuntu24.sh` — builds `ubuntu-24.04-rootfs.ext4` via
  debootstrap (Ubuntu 24.04 minbase + systemd), installing the
  release-built guestd as a systemd service. Note the rootfs must not
  hardcode a network address: hostd seeds per-VM static network config into
  the overlay upper layer at VM creation time.
- `build_initramfs.sh` — packs busybox + `initramfs_init.sh` into
  `initramfs.cpio.gz` (newc cpio, gzipped). `initramfs.cpio.gz` is
  git-ignored as a build artifact.

## Code style guidelines

- Match the existing style: extensive `//!`/`///` module and item docs, and
  explanatory comments for non-obvious mechanisms (networking, overlayfs,
  vsock). Non-trivial modules start with a `//!` doc block describing their
  role — keep these accurate when changing behavior.
- All hostd items are `pub(crate)`; there is no public library API.
- Error handling: crate-wide `thiserror` `Error`/`Result` in
  `hostd/src/error.rs`; API handlers convert errors to the uniform JSON
  error body. Route handlers use the `ApiJson`/`ApiResult` wrappers.
- Newtype IDs (`VmId`, `WorkloadId`, `TapName`) with
  `#[serde(transparent)]`, `Display`, and `new_random()` constructors.
- Networking separation of concerns: `net/state.rs` is pure allocation
  logic (unit-testable), `net/host.rs` is the only place that shells out to
  `ip`/`iptables`, `net/manager.rs` mediates and persists state. Preserve
  this split — do not add shell-outs outside `host.rs`.
- Bash scripts use `set -euo pipefail` and heavily comment the "why";
  follow that pattern.
- Commit history style: short imperative subject lines describing the
  change.

## Security considerations

- The hostd API listens on `0.0.0.0` by default; its only protection is the
  Bearer token from `TIKOVM_HOSTD_API_TOKEN`. Never commit real tokens; the
  scripts use a placeholder `xxx` for local testing only.
- hostd runs as root and spawns Firecracker with `--no-seccomp`
  (`vmm/firecracker/setup.rs`) — be aware of the reduced sandboxing when
  modifying the spawn configuration.
- The guest image sets a default `root:root` password and enables root SSH
  login (`build_rootfs_ubuntu24.sh`); treat guest images as test-only.
- `.gitignore` excludes `.env`; do not write secrets into tracked files.
