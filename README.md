# tikovm

## hostd

`hostd` manages Firecracker microVMs and their host networking. It must run
as root (it creates bridges/TAP devices and iptables NAT rules, and
loop-mounts overlay disks). Use `scripts/run_hostd.sh`, which builds as
the current user and runs the binary via `sudo -E`.

Networking: each project gets its own bridge (`tbr-<project_id>`) with a
subnet carved from `--net-supernet` (default `172.16.0.0/12`,
`--net-subnet-prefix` default `24`). VMs in the same project share the subnet
and reach each other at L2; the host side of the bridge is the gateway (`.1`)
and internet egress is NATed per subnet. A project's bridge/subnet is created
when its first VM is created and torn down when its last VM is destroyed;
allocation state is persisted under the work dir and reconciled on startup.
The guest IP is delivered as a kernel `ip=` boot argument (the guest kernel
has `CONFIG_IP_PNP=y`), so eth0 is configured before init runs, independent
of the guest image's network userspace.

Auto-suspend: a permanent VM created with an `auto_suspend` config is
snapshotted (Firecracker process stopped — the VM consumes no CPU or memory,
only the snapshot files on disk) once it looks idle, and is transparently
restored by the next proxied HTTP request or `POST /{id}/exec`. Two detector
paths decide "idle":

- HTTP: the proxy tracks per-VM request activity; a VM with exposed ports
  suspends after `idle_timeout_secs` without a proxied request.
- non-HTTP: guestd runs the VM's `idle_check_cmd` every
  `check_interval_secs` (exit status 0 = idle, e.g. a script checking for
  established database connections) and forwards the idle event over vsock.

Both paths are gated host-side (permanent mode, `started` state, no
in-flight proxied requests, post-wake cooldown) before the snapshot happens.
`auto_suspend` is only accepted for `permanent` VMs:

```json
"auto_suspend": {
    "idle_timeout_secs": 300,
    "idle_check_cmd": ["/usr/local/sbin/check-idle"],
    "check_interval_secs": 30
}
```

`idle_check_cmd` may be empty (HTTP-only), and a VM with neither exposed
ports nor a check command never suspends.

Block storage: a VM created with a `block_storage` config gets a dedicated
block device (`/dev/vdc`) served by a per-VM `ublk-worker` subprocess
(hostd re-executing itself with a hidden subcommand, driving the kernel
ublk driver). Guest IO is mapped onto fixed-size chunk files under
`<storage-root>/proj-<project_id>/<vm_id>/` (production: an AWS S3 Files
NFS mount, `--storage-root` default `/mnt/s3files/vm_storage`; missing
chunks read as zeros). hostd formats a fresh volume ext4 and seeds a
systemd mount unit into the overlay disk, so the volume is mounted at
`mount_path` with no guest cooperation:

```json
"block_storage": {
    "size_mb": 512,
    "chunk_kb": 1024,
    "mount_path": "/mnt/tikovm-data"
}
```

`chunk_kb` is optional (default 1024; allowed 256/512/1024/2048/4096) and
`mount_path` may be set to `""` to attach the device raw. The volume dies
with the VM (destroy deletes the chunk files); snapshot/restore is
transparent because the worker and device are independent of the
Firecracker process.

Durability/performance contract (measured on S3 Files): a completed guest
fsync means every dirty chunk was fdatasynced (one NFS COMMIT per dirty
chunk, ~9 ms p50 — fine for data volumes, not for fsync-heavy database
primaries). A worker crash fails in-flight IOs with EIO (the guest's ext4
replays its journal, exactly as after a disk power blip) and the device is
transparently recovered by a respawned worker. Expect roughly 300-800
random 4 KiB IOPS and ~750-960 MiB/s sequential writes per volume on S3
Files backing; the host page cache absorbs hot working sets.

Schedule mode: a VM created with `"mode": "schedule"`, a `cmd`, and a
`cron_schedule` (UTC; standard 5-field cron or 6/7 fields with seconds) is
not started on creation. hostd's cron scheduler wakes it on every fire
(start, or restore from its snapshot), runs `cmd` as a workload, then
snapshots it back to `suspended` — so between runs it consumes no CPU or
memory, only the snapshot files on disk. An optional `timeout_secs` stops a
run that overruns (SIGTERM, then SIGKILL in the guest); a fire arriving
while the previous run is still active is skipped, never queued. Each run
is a regular workload tagged `"origin": "schedule"`, so run history and
captured logs are queryable through the workloads API
(`GET /api/vms/{id}/workloads[/{workload_id}/logs]`):

```json
"mode": "schedule",
"cmd": ["sh", "-c", "/usr/local/bin/nightly-job"],
"cron_schedule": "0 3 * * *",
"timeout_secs": 3600
```

## Node.js client

The official Node.js/TypeScript client for the hostd API lives in
`clients/node/` (npm package `tikovm`). It wraps the `/api` endpoints behind
a `Tikovm` client with a `client.vms` namespace and per-VM resource objects
for VM lifecycle management (create/get/list/delete, pause/resume, snapshot/
restore, exec), the read-only per-VM network config, and the exposed-port
registry with proxy-token minting. Zero runtime dependencies (native
`fetch`, Node >= 18); see `clients/node/README.md` for usage.

