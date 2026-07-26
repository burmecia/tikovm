# tikovm

## hostd

`hostd` manages Firecracker microVMs and their host networking. It must run
as root (it creates bridges/TAP devices and iptables NAT rules, and
loop-mounts overlay disks). Use `hostd/scripts/run_hostd.sh`, which builds as
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
