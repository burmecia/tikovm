//! Firecracker process lifecycle and one-time machine configuration:
//! spawning the `firecracker` binary and driving its pre-boot API setup.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::process;
use tracing::{debug, info};

use crate::error::{Error, Result};
use crate::vmm::vm::{VmInstance, VmInstanceRef};

use super::api::FcApiClient;
use super::vmm::FirecrackerVmm;

impl FirecrackerVmm {
    /// Spawn the Firecracker process for a VM and wait for its API socket
    /// to appear (up to 5s). The child's stderr goes to the VM's error log.
    pub(super) async fn spawn_fc_process(
        &self,
        instance_ref: &VmInstanceRef,
    ) -> Result<process::Child> {
        let (vm_id, socket_path, error_log, vsock_uds_path) = {
            let instance = instance_ref.lock()?;
            (
                instance.vm_id.clone(),
                instance.socket_path.clone(),
                instance.error_log.clone(),
                instance.vsock_uds_path.clone(),
            )
        };

        let _ = fs::remove_file(&socket_path); // clean stale socket
        let _ = fs::remove_file(&vsock_uds_path); // clean stale vsock UDS
        let stderr_file = fs::File::create(&error_log)?;

        debug!(vm_id = %vm_id, "spawning Firecracker");

        let child = process::Command::new(&self.fc_bin)
            .arg("--api-sock")
            .arg(&socket_path)
            .arg("--no-seccomp")
            .arg("--enable-pci")
            .arg("--id")
            .arg(&vm_id)
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr_file))
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| Error::vmm(format!("spawn firecracker: {e}")))?;

        // Wait for the API socket to appear (up to 5s).
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if socket_path.exists() {
                break;
            }
            if Instant::now() > deadline {
                return Err(Error::vmm(format!(
                    "Firecracker API socket {} did not appear within 5s",
                    socket_path.display()
                )));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        info!(vm_id = %vm_id, "Firecracker process spawned");

        Ok(child)
    }

    /// Drive the pre-boot API setup of a freshly spawned Firecracker
    /// process: boot source, machine config, drives, serial, vsock, network.
    pub(super) async fn configure_vm(&self, instance_ref: VmInstanceRef) -> Result<()> {
        let instance = instance_ref.lock()?.clone();
        let client = FcApiClient::new(&instance.socket_path);

        put_boot_source(&client, &instance).await?;
        put_machine_config(&client, &instance).await?;
        put_drive(&client, "rootfs", &instance.rootfs_path, true).await?;

        // Per-VM read-write overlay disk (/dev/vdb). The initramfs mounts it
        // and uses it as the overlayfs upper/work backing store, so it only
        // needs a fresh empty ext4 filesystem sized per the VM config.
        create_overlay_disk(&instance.overlay_disk, instance.vm_config.disk_size_mb).await?;
        // Seed per-VM guest config (e.g. PostgreSQL pg_hba rules, block-volume
        // mount unit) into the disk's upper layer before the drive is attached.
        seed_overlay_disk(&instance).await?;
        put_drive(&client, "overlay", &instance.overlay_disk, false).await?;

        // Optional dedicated block volume (/dev/vdc), served by the VM's
        // ublk worker (see `storage` module). The drive attach order —
        // rootfs, overlay, data — fixes the guest device names vda/vdb/vdc.
        if let Some(block_device) = &instance.block_device {
            put_drive(&client, "data", block_device, false).await?;
        }

        put_serial(&client, &instance).await?;
        put_vsock(&client, &instance).await?;
        put_net_iface(&client, &instance).await?;

        Ok(())
    }
}

/// Configure the boot source (kernel + initramfs). The guest IP is passed
/// as a kernel `ip=` boot arg (the kernel has `CONFIG_IP_PNP=y`), so eth0 is
/// configured before init runs — independent of whether the guest image's
/// network userspace (udev/networkd) works.
async fn put_boot_source(client: &FcApiClient, instance: &VmInstance) -> Result<()> {
    let boot_args = match &instance.net {
        Some(net) => format!(
            "{} ip={}::{}:{}::eth0:off",
            instance.boot_args,
            net.guest_ip,
            net.gateway_ip,
            net.netmask(),
        ),
        None => instance.boot_args.clone(),
    };
    client
        .put(
            "/boot-source",
            &json!({
                "kernel_image_path": instance.kernel_path.to_string_lossy(),
                "boot_args": boot_args,
                "initrd_path": instance.initramfs_path.to_string_lossy(),
            }),
        )
        .await
}

/// Machine configuration (vCPUs, memory).
async fn put_machine_config(client: &FcApiClient, instance: &VmInstance) -> Result<()> {
    let vm_config = &instance.vm_config;
    client
        .put(
            "/machine-config",
            &json!({
                "vcpu_count": vm_config.cpus,
                "mem_size_mib": vm_config.memory_mb,
                "smt": false,
                "track_dirty_pages": false,
                "huge_pages": "None",
            }),
        )
        .await
}

/// Attach a drive. The root device is the shared read-only base image; the
/// overlay drive is the per-VM writable disk.
async fn put_drive(client: &FcApiClient, drive_id: &str, path: &Path, is_root: bool) -> Result<()> {
    client
        .put(
            &format!("/drives/{drive_id}"),
            &json!({
                "drive_id": drive_id,
                "path_on_host": path.to_string_lossy(),
                "is_root_device": is_root,
                "is_read_only": is_root,
                "cache_type": "Unsafe",
                "io_engine": "Async",
            }),
        )
        .await
}

/// Serial console output, written to the VM's serial log on the host.
async fn put_serial(client: &FcApiClient, instance: &VmInstance) -> Result<()> {
    client
        .put(
            "/serial",
            &json!({
                "serial_out_path": instance.serial_log.to_string_lossy(),
            }),
        )
        .await
}

/// Vsock device for the guestd control channel (workload execution).
/// Firecracker creates a Unix listener at `uds_path`; hostd connects to it
/// and issues `CONNECT <port>` to reach guestd inside the guest.
async fn put_vsock(client: &FcApiClient, instance: &VmInstance) -> Result<()> {
    let guest_cid = instance
        .guest_cid
        .ok_or_else(|| Error::vmm(format!("vm {} has no vsock guest CID", instance.vm_id)))?;
    client
        .put(
            "/vsock",
            &json!({
                "vsock_id": "vsock0",
                "guest_cid": guest_cid,
                "uds_path": instance.vsock_uds_path.to_string_lossy(),
            }),
        )
        .await
}

/// Network interface backed by the VM's TAP device.
async fn put_net_iface(client: &FcApiClient, instance: &VmInstance) -> Result<()> {
    let vm_net = instance
        .net
        .clone()
        .ok_or_else(|| Error::net(format!("vm {} has no network allocation", instance.vm_id)))?;
    client
        .put(
            "/network-interfaces/eth0",
            &json!({
                "iface_id": "eth0",
                "guest_mac": vm_net.guest_mac,
                "host_dev_name": vm_net.tap_name.to_string(),
            }),
        )
        .await
}

/// Create a fresh ext4 disk image of `size_mb` at `path` for the VM's
/// overlayfs upper/work backing store. The file is sparse, so only the
/// filesystem metadata actually consumes host disk space.
async fn create_overlay_disk(path: &Path, size_mb: u32) -> Result<()> {
    let file = fs::File::create(path)?;
    file.set_len(u64::from(size_mb) * 1024 * 1024)?;
    drop(file);

    let status = process::Command::new("mkfs.ext4")
        .args(["-F", "-q", "-m", "0"])
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map_err(|e| Error::vmm(format!("spawn mkfs.ext4: {e}")))?;
    if !status.success() {
        return Err(Error::vmm(format!(
            "mkfs.ext4 {} failed: {status}",
            path.display()
        )));
    }
    Ok(())
}

/// Seed per-VM guest configuration into the overlay disk's upper layer.
///
/// The overlay disk is a fresh ext4 whose `upper/` tree shadows the shared
/// read-only base image via overlayfs (see `scripts/initramfs_init.sh`), so
/// files written here before the first boot are how hostd injects per-VM
/// config the base image cannot hardcode. The disk is loop-mounted, seeded,
/// and unmounted again before Firecracker attaches it.
///
/// Two kinds of seeding today:
///
/// - `PostgreSQL` images: a `pg_hba` rule scoped to the VM's project
///   subnet, so the cluster (the base image makes it listen on all
///   interfaces) accepts password connections from the host's bridge IP
///   and sibling VMs in the same project — and from nowhere else. The base
///   image's `pg_hba.conf` pulls the rule in via
///   `include_dir '/etc/postgresql/16/main/pg_hba.d'` (needs `PostgreSQL`
///   16).
/// - Block-storage volumes (`VmConfig::block_storage` with a non-empty
///   `mount_path`): a systemd mount unit for the data drive (/dev/vdc,
///   found by the ext4 label hostd formatted it with), so the volume is
///   mounted at `mount_path` on every boot without guest cooperation.
async fn seed_overlay_disk(instance: &VmInstance) -> Result<()> {
    let needs_pg_hba = instance.vm_config.image == "postgres-16";
    let mount_path = instance
        .vm_config
        .block_storage
        .as_ref()
        .map(|bs| bs.mount_path.as_str())
        .filter(|p| !p.is_empty());
    if !needs_pg_hba && mount_path.is_none() {
        return Ok(());
    }

    let mount_point = instance.work_dir.join("seed.mnt");
    fs::create_dir_all(&mount_point)?;

    let disk = instance.overlay_disk.to_string_lossy();
    let mnt = mount_point.to_string_lossy();
    run_tool("mount", &["-o", "loop", disk.as_ref(), mnt.as_ref()]).await?;

    // Unmount no matter how the seeding itself fares; a disk left mounted
    // would still be attached to the VM, with hostd's mount leaking.
    let seed_result = async {
        if needs_pg_hba {
            let net = instance
                .net
                .as_ref()
                .ok_or_else(|| {
                    Error::net(format!("vm {} has no network allocation", instance.vm_id))
                })?;
            let hba_dir = mount_point.join("upper/etc/postgresql/16/main/pg_hba.d");
            fs::create_dir_all(&hba_dir)?;
            fs::write(
                hba_dir.join("00-tikovm.conf"),
                format!(
                    "# Seeded by hostd at VM creation: password auth only from the\n\
                     # VM's project subnet (the host's bridge IP + sibling VMs).\n\
                     host all all {} scram-sha-256\n",
                    net.subnet
                ),
            )?;
        }

        if let Some(where_path) = mount_path {
            let unit_name = format!("{}.mount", systemd_escape_path(where_path));
            let unit_dir = mount_point.join("upper/etc/systemd/system");
            fs::create_dir_all(unit_dir.join("multi-user.target.wants"))?;
            fs::write(
                unit_dir.join(&unit_name),
                format!(
                    "# Seeded by hostd at VM creation: mounts the VM's dedicated\n\
                     # block volume (the /dev/vdc drive, ext4-formatted by hostd).\n\
                     [Unit]\n\
                     Description=tikovm block storage volume\n\
                     Before=local-fs.target\n\
                     \n\
                     [Mount]\n\
                     What=/dev/disk/by-label/tikovm-data\n\
                     Where={where_path}\n\
                     Type=ext4\n\
                     \n\
                     [Install]\n\
                     WantedBy=multi-user.target\n"
                ),
            )?;
            std::os::unix::fs::symlink(
                format!("../{unit_name}"),
                unit_dir.join("multi-user.target.wants").join(&unit_name),
            )?;
        }
        Ok::<_, Error>(())
    }
    .await;

    let umount_result = run_tool("umount", &[mnt.as_ref()]).await;
    let _ = fs::remove_dir(&mount_point);

    seed_result?;
    umount_result
}

/// `systemd-escape --path` for the mount_path character set validated at
/// VM create (`[a-z0-9/._-]`): '/' separates components (and becomes '-'
/// in the name), '-' is escaped `\x2d`, and a '.' is escaped `\x2e` only
/// as the very first character. The mount unit name must match this
/// exactly or systemd refuses the unit ("Where= setting doesn't match
/// unit name").
fn systemd_escape_path(path: &str) -> String {
    let mut out = String::new();
    for c in path.trim_matches('/').chars() {
        match c {
            '/' => out.push('-'),
            '-' => out.push_str("\\x2d"),
            _ => out.push(c),
        }
    }
    if let Some(rest) = out.strip_prefix('.') {
        out = format!("\\x2e{rest}");
    }
    out
}

/// Run a root-requiring host tool (mount/umount) and check its exit status.
async fn run_tool(tool: &str, args: &[&str]) -> Result<()> {
    let status = process::Command::new(tool)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map_err(|e| Error::vmm(format!("spawn {tool}: {e}")))?;
    if !status.success() {
        return Err(Error::vmm(format!("{tool} {args:?} failed: {status}")));
    }
    Ok(())
}

/// Locate a binary: the `env_var` override wins, then a PATH search, then
/// the bare name (left for the OS to resolve at spawn time).
pub(super) fn from_path_or_env(binary: &str, env_var: &str) -> PathBuf {
    if let Some(path) = env::var_os(env_var) {
        return PathBuf::from(path);
    }

    if let Some(path) = env::var_os("PATH") {
        for dir in env::split_paths(&path) {
            let candidate = dir.join(binary);
            if candidate.is_file() {
                return candidate;
            }
        }
    }

    PathBuf::from(binary)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Expected values verified against `systemd-escape --path`.
    #[test]
    fn systemd_path_escaping() {
        assert_eq!(systemd_escape_path("/mnt/tikovm-data"), "mnt-tikovm\\x2ddata");
        assert_eq!(systemd_escape_path("/data"), "data");
        assert_eq!(systemd_escape_path("/a/b-c"), "a-b\\x2dc");
        assert_eq!(systemd_escape_path("/mnt/data.d"), "mnt-data.d");
        assert_eq!(systemd_escape_path("/x/.hidden"), "x-.hidden");
        assert_eq!(systemd_escape_path("/.hidden"), "\\x2ehidden");
    }
}
