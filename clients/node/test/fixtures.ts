import type { VmConfig, VmInstance, VmState } from '../src/types.js';

/** A minimal, valid VmInstance matching hostd's serialized shape. */
export function makeVmInstance(
  id: string,
  state: VmState = 'started',
  overrides: Partial<VmInstance> = {},
): VmInstance {
  const config: VmConfig = {
    name: `vm ${id}`,
    project_id: 123,
    mode: 'ephemeral',
    image: 'ubuntu-24',
    cpus: 1,
    memory_mb: 512,
    disk_size_mb: 1024,
    network_config: {
      allow_internet: false,
      exposed_ports: [],
      egress: [],
      public_access: false,
    },
    ssh_access: false,
    env: [],
    cmd: [],
    services: [],
    cron_schedule: null,
    timeout_secs: null,
    tags: [],
    auto_suspend: null,
    block_storage: null,
  };
  return {
    vm_id: id,
    state,
    work_dir: `/tmp/tikovm/${id}`,
    socket_path: `/tmp/tikovm/${id}/${id}.socket`,
    kernel_path: '/assets/vmlinux.bin',
    initramfs_path: '/assets/initramfs.cpio.gz',
    boot_args: 'console=ttyS0 reboot=k panic=1 pci=on nomodules',
    rootfs_path: '/assets/ubuntu-24.04-rootfs.ext4',
    overlay_disk: `/tmp/tikovm/${id}/${id}.overlay.ext4`,
    block_device: null,
    net: {
      tap_name: `tap-${id}`,
      guest_ip: '172.16.0.2',
      gateway_ip: '172.16.0.1',
      subnet: '172.16.0.0/24',
      guest_mac: 'AA:FC:AC:10:00:02',
    },
    guest_cid: 3,
    vsock_uds_path: `/tmp/tikovm/${id}/${id}.vsock`,
    snapshot: null,
    serial_log: `/tmp/tikovm/${id}/${id}.serial.log`,
    error_log: `/tmp/tikovm/${id}/${id}.stderr.log`,
    created_at: '2026-08-10T00:00:00Z',
    vm_config: config,
    ...overrides,
  };
}
