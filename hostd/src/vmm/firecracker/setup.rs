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
use crate::vmm::vm::VmInstanceRef;

use super::api::FcApiClient;
use super::vmm::FirecrackerVmm;

impl FirecrackerVmm {
    pub(super) fn spawn_fc_process(&self, instance_ref: VmInstanceRef) -> Result<process::Child> {
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
            std::thread::sleep(Duration::from_millis(50));
        }

        info!(vm_id = %vm_id, "Firecracker process spawned");

        Ok(child)
    }

    pub(super) async fn configure_vm(&self, instance_ref: VmInstanceRef) -> Result<()> {
        let instance = instance_ref.lock()?.clone();
        let vm_config = &instance.vm_config;
        let client = FcApiClient::new(&instance.socket_path);

        // Configure boot source (kernel + initramfs). The guest IP is passed
        // as a kernel `ip=` boot arg (the kernel has CONFIG_IP_PNP=y), so
        // eth0 is configured before init runs — independent of whether the
        // guest image's network userspace (udev/networkd) works.
        let boot_args = match &instance.net {
            Some(net) => format!(
                "{} ip={}::{}:{}::eth0:off",
                instance.boot_args,
                net.guest_ip,
                net.gateway_ip,
                net.netmask()?,
            ),
            None => instance.boot_args.clone(),
        };
        let boot_source = json!({
            "kernel_image_path": instance.kernel_path.to_string_lossy(),
            "boot_args": boot_args,
            "initrd_path": instance.initramfs_path.to_string_lossy(),
        });
        client.put("/boot-source", &boot_source).await?;

        // Machine configuration.
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
            .await?;

        // Configure the rootfs drive.
        client
            .put(
                "/drives/rootfs",
                &json!({
                    "drive_id": "rootfs",
                    "path_on_host": instance.rootfs_path.to_string_lossy(),
                    "is_root_device": true,
                    "is_read_only": true,
                    "cache_type": "Unsafe",
                    "io_engine": "Async",
                }),
            )
            .await?;

        // Per-VM read-write overlay disk (/dev/vdb). The initramfs mounts it
        // and uses it as the overlayfs upper/work backing store, so it only
        // needs a fresh empty ext4 filesystem sized per the VM config.
        create_overlay_disk(&instance.overlay_disk, vm_config.disk_size_mb).await?;
        client
            .put(
                "/drives/overlay",
                &json!({
                    "drive_id": "overlay",
                    "path_on_host": instance.overlay_disk.to_string_lossy(),
                    "is_root_device": false,
                    "is_read_only": false,
                    "cache_type": "Unsafe",
                    "io_engine": "Async",
                }),
            )
            .await?;

        // Serial console output.
        client
            .put(
                "/serial",
                &json!({
                    "serial_out_path": instance.serial_log.to_string_lossy(),
                }),
            )
            .await?;

        // Vsock device for the guestd control channel (workload execution).
        // Firecracker creates a Unix listener at uds_path; hostd connects to
        // it and issues `CONNECT <port>` to reach guestd inside the guest.
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
            .await?;

        // Network interface backed by the VM's TAP device.
        let vm_net = instance.net.clone().ok_or_else(|| {
            Error::net(format!("vm {} has no network allocation", instance.vm_id))
        })?;
        client
            .put(
                "/network-interfaces/eth0",
                &json!({
                    "iface_id": "eth0",
                    "guest_mac": vm_net.guest_mac,
                    "host_dev_name": vm_net.tap_name.to_string(),
                }),
            )
            .await?;

        Ok(())
    }
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
