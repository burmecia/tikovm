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
        use VmState::{
            Created, Creating, Destroyed, Destroying, Paused, Pausing, Restoring, Resuming,
            Started, Starting, Suspended, Suspending,
        };
        matches!(
            (self, next),
            (Creating | Starting, Created)
                | (Creating | Destroying, Destroyed)
                | (Created, Starting | Destroying)
                | (
                    Starting | Pausing | Resuming | Suspending | Restoring,
                    Started
                )
                | (Started, Pausing | Suspending | Destroying)
                | (Pausing | Resuming, Paused)
                | (Paused, Resuming | Suspending | Destroying)
                | (Suspending | Restoring, Suspended)
                | (Suspended, Restoring | Destroying)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub(crate) enum VmMode {
    #[default]
    Ephemeral,
    Permanent,
    Schedule,
}

/// Auto-suspend policy for a `VmMode::Permanent` VM: when the VM looks idle
/// (see the two detector paths below), hostd snapshots it and kills the
/// Firecracker process, and a later proxy request or exec restores it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AutoSuspendConfig {
    /// HTTP detector path: suspend after this many seconds without a
    /// proxied request. Also used as the post-wake cooldown before any
    /// suspend, so a just-restored VM cannot flap straight back down.
    pub idle_timeout_secs: u64,
    /// Non-HTTP detector path: a program inside the guest that guestd runs
    /// every `check_interval_secs`; exit status 0 reports "idle" (e.g. a
    /// script checking for established Postgres connections). Empty
    /// disables the guest-side detector.
    #[serde(default)]
    pub idle_check_cmd: Vec<String>,
    /// How often guestd runs `idle_check_cmd`.
    #[serde(default = "default_check_interval_secs")]
    pub check_interval_secs: u64,
}

fn default_check_interval_secs() -> u64 {
    30
}

/// Optional dedicated block volume for a VM: a ublk device backed by
/// chunk files on the storage root (see `storage` module), attached as
/// `/dev/vdc`, formatted ext4 by hostd, and mounted in the guest at
/// `mount_path` by a seeded systemd unit. The volume dies with the VM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct BlockStorageConfig {
    /// Volume size in MiB (minimum 128).
    pub size_mb: u32,
    /// Chunk size in KiB; one of 256/512/1024/2048/4096. Default 1024 —
    /// see `storage::volume` for why.
    #[serde(default)]
    pub chunk_kb: Option<u32>,
    /// Guest mount point for the volume. Empty string attaches the device
    /// raw (no filesystem assumptions, no mount unit).
    #[serde(default = "default_mount_path")]
    pub mount_path: String,
}

fn default_mount_path() -> String {
    "/mnt/tikovm-data".to_string()
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
    /// Schedule-mode command: run inside the guest on every cron fire (the
    /// VM is woken for the run and suspended again afterwards). Required for
    /// `VmMode::Schedule`, rejected for other modes.
    #[serde(default)]
    pub cmd: Vec<String>,
    #[serde(default)]
    pub services: Vec<String>,
    /// Schedule-mode cron expression (UTC). Standard 5-field cron and
    /// 6/7-field expressions with a seconds field are both accepted.
    /// Required for `VmMode::Schedule`, rejected for other modes.
    #[serde(default)]
    pub cron_schedule: Option<String>,
    /// Schedule-mode run timeout: if a scheduled `cmd` is still running this
    /// many seconds after it started, hostd stops it (SIGTERM, then SIGKILL
    /// in the guest) and suspends the VM anyway. Unset means no timeout.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Auto-suspend policy; only valid for `VmMode::Permanent` VMs
    /// (enforced at create time).
    #[serde(default)]
    pub auto_suspend: Option<AutoSuspendConfig>,
    /// Dedicated block volume (ublk, chunk-backed); see `storage` module.
    #[serde(default)]
    pub block_storage: Option<BlockStorageConfig>,
}

impl VmConfig {
    pub(crate) fn rootfs_file(&self) -> Result<String> {
        match self.image.as_str() {
            "node-22" => Ok("node-22-rootfs.ext4".to_string()),
            "postgres-16" => Ok("postgres-16-rootfs.ext4".to_string()),
            "python-3.12" => Ok("python-3.12-rootfs.ext4".to_string()),
            "s3files" => Ok("s3files-rootfs.ext4".to_string()),
            "tiko-postgres" => Ok("tiko-postgres-rootfs.ext4".to_string()),
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
    /// Optional dedicated block volume device (`/dev/ublkbN` -> /dev/vdc),
    /// set by `create_vm` when `VmConfig::block_storage` is configured.
    pub block_device: Option<PathBuf>,

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
            block_device: None,
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

    pub(crate) fn cleanup_runtime_artifacts(&self) {
        fs::remove_dir_all(&self.work_dir).ok(); // ignore errors
    }
}

pub(crate) type VmInstanceRef = Arc<Mutex<VmInstance>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_suspend_defaults() {
        let config: AutoSuspendConfig =
            serde_json::from_str(r#"{"idle_timeout_secs": 300}"#).unwrap();
        assert_eq!(config.idle_timeout_secs, 300);
        assert!(config.idle_check_cmd.is_empty());
        assert_eq!(config.check_interval_secs, 30);
    }

    #[test]
    fn vm_config_without_auto_suspend() {
        let config: VmConfig = serde_json::from_str(
            r#"{"name":"vm","project_id":1,"mode":"permanent","image":"ubuntu-24",
                "cpus":1,"memory_mb":256,"disk_size_mb":1024,
                "network_config":{"allow_internet":false,"exposed_ports":[]},"ssh_access":false}"#,
        )
        .unwrap();
        assert!(config.auto_suspend.is_none());
    }

    #[test]
    fn vm_config_with_auto_suspend() {
        let config: VmConfig = serde_json::from_str(
            r#"{"name":"vm","project_id":1,"mode":"permanent","image":"ubuntu-24",
                "cpus":1,"memory_mb":256,"disk_size_mb":1024,
                "network_config":{"allow_internet":false,"exposed_ports":[]},"ssh_access":false,
                "auto_suspend":{"idle_timeout_secs":60,
                                "idle_check_cmd":["/check"],"check_interval_secs":5}}"#,
        )
        .unwrap();
        let auto_suspend = config.auto_suspend.unwrap();
        assert_eq!(auto_suspend.idle_timeout_secs, 60);
        assert_eq!(auto_suspend.idle_check_cmd, vec!["/check".to_string()]);
        assert_eq!(auto_suspend.check_interval_secs, 5);
    }

    #[test]
    fn vm_config_block_storage_defaults() {
        let config: VmConfig = serde_json::from_str(
            r#"{"name":"vm","project_id":1,"mode":"permanent","image":"ubuntu-24",
                "cpus":1,"memory_mb":256,"disk_size_mb":1024,
                "network_config":{"allow_internet":false,"exposed_ports":[]},"ssh_access":false,
                "block_storage":{"size_mb":512}}"#,
        )
        .unwrap();
        let bs = config.block_storage.unwrap();
        assert_eq!(bs.size_mb, 512);
        assert_eq!(bs.chunk_kb, None);
        assert_eq!(bs.mount_path, "/mnt/tikovm-data");
    }

    #[test]
    fn vm_config_block_storage_explicit() {
        let config: VmConfig = serde_json::from_str(
            r#"{"name":"vm","project_id":1,"mode":"permanent","image":"ubuntu-24",
                "cpus":1,"memory_mb":256,"disk_size_mb":1024,
                "network_config":{"allow_internet":false,"exposed_ports":[]},"ssh_access":false,
                "block_storage":{"size_mb":2048,"chunk_kb":4096,"mount_path":""}}"#,
        )
        .unwrap();
        let bs = config.block_storage.unwrap();
        assert_eq!(bs.chunk_kb, Some(4096));
        assert_eq!(bs.mount_path, "");
    }

    #[test]
    fn vm_config_without_block_storage() {
        let config: VmConfig = serde_json::from_str(
            r#"{"name":"vm","project_id":1,"mode":"permanent","image":"ubuntu-24",
                "cpus":1,"memory_mb":256,"disk_size_mb":1024,
                "network_config":{"allow_internet":false,"exposed_ports":[]},"ssh_access":false}"#,
        )
        .unwrap();
        assert!(config.block_storage.is_none());
    }

    #[test]
    fn vm_config_schedule_defaults() {
        let config: VmConfig = serde_json::from_str(
            r#"{"name":"vm","project_id":1,"mode":"schedule","image":"ubuntu-24",
                "cpus":1,"memory_mb":256,"disk_size_mb":1024,
                "network_config":{"allow_internet":false,"exposed_ports":[]},"ssh_access":false}"#,
        )
        .unwrap();
        assert_eq!(config.mode, VmMode::Schedule);
        assert!(config.cmd.is_empty());
        assert!(config.cron_schedule.is_none());
        assert!(config.timeout_secs.is_none());
    }

    #[test]
    fn vm_config_schedule_round_trip() {
        let config: VmConfig = serde_json::from_str(
            r#"{"name":"vm","project_id":1,"mode":"schedule","image":"ubuntu-24",
                "cpus":1,"memory_mb":256,"disk_size_mb":1024,
                "network_config":{"allow_internet":false,"exposed_ports":[]},"ssh_access":false,
                "cmd":["/run.sh"],"cron_schedule":"*/5 * * * *","timeout_secs":120}"#,
        )
        .unwrap();
        assert_eq!(config.cmd, vec!["/run.sh".to_string()]);
        assert_eq!(config.cron_schedule.as_deref(), Some("*/5 * * * *"));
        assert_eq!(config.timeout_secs, Some(120));

        let json = serde_json::to_string(&config).unwrap();
        let back: VmConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.mode, VmMode::Schedule);
        assert_eq!(back.cmd, config.cmd);
        assert_eq!(back.cron_schedule, config.cron_schedule);
        assert_eq!(back.timeout_secs, config.timeout_secs);
    }
}
