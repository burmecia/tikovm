use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::fs;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::utils::random_id;
use crate::error::{Error, Result};
use crate::net::{NetworkConfig, VmNet};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct VmId(pub String);

impl VmId {
    pub(crate) fn new_random(project_id: u64) -> Self {
        Self(random_id(&format!("vm-{project_id}")))
    }
}

impl std::fmt::Display for VmId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl Deref for VmId {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AsRef<str> for VmId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl AsRef<OsStr> for VmId {
    fn as_ref(&self) -> &OsStr {
        self.0.as_ref()
    }
}

impl From<String> for VmId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for VmId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub(crate) enum VmState {
    // --- transitional ---
    #[default]
    Creating,
    Starting,
    Pausing,
    Resuming,
    Suspending,
    Restoring,
    Destroying,
    // --- stable ---
    Created,
    Started,
    Paused,
    Suspended,
    Destroyed,
}

impl VmState {
    /// Valid transitions: stable -> transitional -> stable, where the second
    /// step either completes the operation or rolls back to the previous
    /// stable state on failure.
    fn can_transition_to(self, next: VmState) -> bool {
        use VmState::*;
        matches!(
            (self, next),
            (Creating, Created)
                | (Creating, Destroyed)
                | (Created, Starting)
                | (Created, Destroying)
                | (Starting, Started)
                | (Starting, Created)
                | (Started, Pausing)
                | (Started, Suspending)
                | (Started, Destroying)
                | (Pausing, Paused)
                | (Pausing, Started)
                | (Paused, Resuming)
                | (Paused, Suspending)
                | (Paused, Destroying)
                | (Resuming, Started)
                | (Resuming, Paused)
                | (Suspending, Suspended)
                | (Suspending, Started)
                | (Suspended, Restoring)
                | (Suspended, Destroying)
                | (Restoring, Started)
                | (Restoring, Suspended)
                | (Destroying, Destroyed)
        )
    }

    /// Move to `next`, rejecting transitions outside the state machine.
    pub(crate) fn transition(&mut self, next: VmState) -> crate::error::Result<()> {
        if self.can_transition_to(next) {
            *self = next;
            Ok(())
        } else {
            Err(crate::error::Error::InvalidStateTransition {
                from: *self,
                to: next,
            })
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub(crate) enum VmMode {
    #[default]
    Ephemeral,
    Permanent,
    Schedule,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct EnvVar {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct VmSnapshot {
    pub state_path: PathBuf,
    pub mem_path: PathBuf,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl VmSnapshot {
    fn new(vm_id: &VmId, work_dir: impl AsRef<Path>) -> Self {
        let state_path = work_dir.as_ref().join(format!("{vm_id}_snapshot.state"));
        let mem_path = work_dir.as_ref().join(format!("{vm_id}_snapshot.mem"));
        Self {
            state_path,
            mem_path,
            created_at: chrono::Utc::now(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct VmConfig {
    pub name: String,
    pub project_id: u64,
    pub mode: VmMode,
    pub image: String,
    pub cpus: u32,
    pub memory_mb: u32,
    pub disk_size_mb: u32,
    pub network_config: NetworkConfig,
    pub ssh_access: bool,
    #[serde(default)]
    pub env: Vec<EnvVar>,
    #[serde(default)]
    pub cmd: Vec<String>,
    #[serde(default)]
    pub services: Vec<String>,
    #[serde(default)]
    pub cron_schedule: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

impl VmConfig {
    pub(crate) fn rootfs_file(&self) -> Result<String> {
        match self.image.as_str() {
            "node-22" => Ok("node-22-rootfs.ext4".to_string()),
            "ubuntu-24" => Ok("ubuntu-24.04-rootfs.ext4".to_string()),
            _ => Err(Error::InvalidImage(self.image.clone())),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct VmInstance {
    pub vm_id: VmId,
    pub state: VmState,
    pub work_dir: PathBuf,
    pub socket_path: PathBuf,

    // Booting setup
    pub kernel_path: PathBuf,
    pub initramfs_path: PathBuf,
    pub boot_args: String,

    // Storage setup
    pub rootfs_path: PathBuf, // Read-only rootfs backing the overlayfs lower dir (/dev/vda)
    pub overlay_disk: PathBuf, // Per-VM rw disk backing the overlayfs upper/work dirs (/dev/vdb)

    // Networking setup (allocated during create_vm, before Firecracker starts)
    pub net: Option<VmNet>,

    // Vsock setup (guest_cid allocated during create_vm, before Firecracker starts)
    pub guest_cid: Option<u32>,
    pub vsock_uds_path: PathBuf,

    // Snapshot setup
    pub snapshot: Option<VmSnapshot>,

    // Logging setup
    pub serial_log: PathBuf,
    pub error_log: PathBuf,

    pub created_at: chrono::DateTime<chrono::Utc>,

    /// Configuration for the VM.
    pub vm_config: VmConfig,
}

impl VmInstance {
    pub(crate) fn new(
        config: &VmConfig,
        assets_dir: impl AsRef<Path>,
        work_dir: impl AsRef<Path>,
    ) -> Result<Self> {
        let vm_id = VmId::new_random(config.project_id);
        let work_dir = work_dir.as_ref().join(&*vm_id);

        let kernel_path = assets_dir.as_ref().join("vmlinux.bin");
        let initramfs_path = assets_dir.as_ref().join("initramfs.cpio.gz");
        let boot_args = "console=ttyS0 reboot=k panic=1 pci=on nomodules".to_string();

        let rootfs_path = assets_dir.as_ref().join(config.rootfs_file()?);
        let overlay_disk = work_dir.join(format!("{vm_id}.overlay.ext4"));

        let socket_path = work_dir.join(format!("{vm_id}.socket"));
        let vsock_uds_path = work_dir.join(format!("{vm_id}.vsock"));

        let serial_log = work_dir.join(format!("{vm_id}.serial.log"));
        let error_log = work_dir.join(format!("{vm_id}.stderr.log"));

        Ok(Self {
            vm_id: vm_id.clone(),
            state: VmState::Creating,
            work_dir,
            socket_path,
            kernel_path,
            initramfs_path,
            boot_args,
            rootfs_path,
            overlay_disk,
            snapshot: None,
            net: None,
            guest_cid: None,
            vsock_uds_path,
            serial_log,
            error_log,
            created_at: chrono::Utc::now(),
            vm_config: config.clone(),
        })
    }

    pub(crate) fn into_ref(self) -> VmInstanceRef {
        Arc::new(Mutex::new(self))
    }

    pub(crate) fn new_snapshot(&self) -> VmSnapshot {
        VmSnapshot::new(&self.vm_id, &self.work_dir)
    }

    pub(crate) fn cleanup_runtime_artifacts(&self) -> Result<()> {
        fs::remove_dir_all(&self.work_dir).ok(); // ignore errors
        Ok(())
    }
}

pub(crate) type VmInstanceRef = Arc<Mutex<VmInstance>>;
