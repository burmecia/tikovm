//! Serde models mirroring the subset of hostd's `GET /api/vms` response that
//! `vmtop` displays, plus the pure helpers used to shape them for the UI.
//!
//! Field names, enum variants, and serialized values must stay in sync with
//! `hostd/src/vmm/vm.rs` (`VmInstance`/`VmConfig`/`VmState`/`VmMode`) and
//! `hostd/src/net/types.rs` (`VmNet`/`NetworkConfig`). Serde ignores unknown
//! fields, so only the parts the monitor cares about are declared.

use std::net::IpAddr;

use chrono::{DateTime, Utc};
use serde::Deserialize;

/// One VM as returned by `GET /api/vms`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Vm {
    pub vm_id: String,
    pub state: VmState,
    /// Allocated network identity; absent only for a VM whose create request
    /// never reached the network allocation step.
    #[serde(default)]
    pub net: Option<VmNet>,
    /// Snapshot presence indicates a `Suspended` VM (auto-suspend or
    /// schedule mode parked between runs).
    #[serde(default)]
    pub snapshot: Option<VmSnapshot>,
    pub created_at: DateTime<Utc>,
    pub vm_config: VmConfig,
}

impl Vm {
    /// The VM's guest IP address, or `None` before network allocation.
    pub(crate) fn guest_ip(&self) -> Option<IpAddr> {
        self.net.as_ref().map(|n| n.guest_ip)
    }

    /// Number of ports exposed through the proxy (count, not the labels).
    pub(crate) fn exposed_port_count(&self) -> usize {
        self.vm_config.network_config.exposed_ports.len()
    }

    /// Substring-matches the filter against every human-visible field.
    pub(crate) fn matches(&self, needle: &str) -> bool {
        let needle = needle.to_lowercase();
        self.vm_id.to_lowercase().contains(&needle)
            || self.vm_config.name.to_lowercase().contains(&needle)
            || self.vm_config.image.to_lowercase().contains(&needle)
            || self.state.to_string().contains(&needle)
            || self
                .guest_ip()
                .is_some_and(|ip| ip.to_string().contains(&needle))
            || self
                .vm_config
                .tags
                .iter()
                .any(|t| t.to_lowercase().contains(&needle))
            || self
                .net
                .as_ref()
                .is_some_and(|n| n.subnet.contains(&needle))
    }
}

/// The VM's create-time configuration (also the `payload` echoed by
/// `POST /api/vms`).
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct VmConfig {
    pub name: String,
    pub project_id: u64,
    pub mode: VmMode,
    pub image: String,
    /// *Configured* vCPUs — hostd does not expose live CPU usage.
    pub cpus: u32,
    /// *Configured* memory in MiB — hostd does not expose live RSS.
    pub memory_mb: u32,
    /// Size of the per-VM writable overlay disk, in MiB.
    pub disk_size_mb: u32,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub network_config: NetworkConfig,
    #[serde(default)]
    pub auto_suspend: Option<AutoSuspendConfig>,
    #[serde(default)]
    pub block_storage: Option<BlockStorageConfig>,
}

/// hostd `VmMode` value (`vm.rs`: ephemeral | permanent | schedule).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum VmMode {
    Ephemeral,
    Permanent,
    Schedule,
}

impl std::fmt::Display for VmMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

/// hostd `VmState` variants (`vm.rs`), serialized snake_case.
///
/// There is no `VmStateDetail` — `state` is a flat string in the API. The
/// variants span the transitional (`Creating` … `Destroying`) and stable
/// states (`Created`/`Started`/`Paused`/`Suspended`/`Destroyed`). Display
/// order is not meaningful; `view` sorts by an explicit rank.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum VmState {
    Creating,
    Starting,
    Pausing,
    Resuming,
    Suspending,
    Restoring,
    Destroying,
    Created,
    Started,
    Paused,
    Suspended,
    Destroyed,
}

impl std::fmt::Display for VmState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            VmState::Creating => "creating",
            VmState::Starting => "starting",
            VmState::Pausing => "pausing",
            VmState::Resuming => "resuming",
            VmState::Suspending => "suspending",
            VmState::Restoring => "restoring",
            VmState::Destroying => "destroying",
            VmState::Created => "created",
            VmState::Started => "started",
            VmState::Paused => "paused",
            VmState::Suspended => "suspended",
            VmState::Destroyed => "destroyed",
        };
        write!(f, "{s}")
    }
}

/// Allocated network identity for a running/created VM.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct VmNet {
    pub tap_name: String,
    pub guest_ip: IpAddr,
    pub gateway_ip: IpAddr,
    /// NAT subnet of the VM's project, CIDR string e.g. `172.16.115.0/24`.
    pub subnet: String,
}

/// Snapshot state present while a VM is `Suspended`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct VmSnapshot {
    pub created_at: DateTime<Utc>,
}

/// Per-VM network config: internet allowance and the exposed-ports registry.
#[derive(Debug, Clone, Deserialize, Default)]
pub(crate) struct NetworkConfig {
    /// Ports (with labels) the VM exposes for its HTTP workloads.
    #[serde(default)]
    pub exposed_ports: Vec<ExposedPort>,
}

/// One registered exposed port (`{port, label}`).
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ExposedPort {
    pub port: u16,
}

/// Auto-suspend policy for a `permanent` VM.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct AutoSuspendConfig {
    pub idle_timeout_secs: u64,
}

/// Dedicated block volume (ublk, chunk-backed).
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct BlockStorageConfig {
    pub size_mb: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vm_json() -> serde_json::Value {
        serde_json::json!([{
            "vm_id": "vm-123-a1b2c3",
            "state": "started",
            "work_dir": "/tmp/tikovm/vm-123-a1b2c3",
            "socket_path": "/tmp/tikovm/vm-123-a1b2c3.socket",
            "kernel_path": "/assets/vmlinux.bin",
            "initramfs_path": "/assets/initramfs.cpio.gz",
            "boot_args": "console=ttyS0 reboot=k",
            "rootfs_path": "/assets/ubuntu-24.04-rootfs.ext4",
            "overlay_disk": "/tmp/tikovm/vm-123-a1b2c3.overlay.ext4",
            "block_device": null,
            "net": {
                "tap_name": "tap-123-a1b2c3",
                "guest_ip": "172.16.115.2",
                "gateway_ip": "172.16.115.1",
                "subnet": "172.16.115.0/24",
                "guest_mac": "AA:FC:AC:10:73:02"
            },
            "guest_cid": 1,
            "vsock_uds_path": "/tmp/tikovm/vm-123-a1b2c3.vsock",
            "snapshot": null,
            "serial_log": "/tmp/tikovm/vm-123-a1b2c3.serial.log",
            "error_log": "/tmp/tikovm/vm-123-a1b2c3.stderr.log",
            "created_at": "2026-08-09T12:00:00.000000000Z",
            "vm_config": {
                "name": "web",
                "project_id": 123,
                "mode": "permanent",
                "image": "ubuntu-24",
                "cpus": 2,
                "memory_mb": 512,
                "disk_size_mb": 1024,
                "network_config": {
                    "allow_internet": true,
                    "exposed_ports": [{"port": 8080, "label": "web"}],
                    "egress": [],
                    "public_access": false
                },
                "ssh_access": false,
                "env": [], "cmd": [], "services": [],
                "cron_schedule": null, "timeout_secs": null,
                "tags": ["prod", "api"],
                "auto_suspend": {
                    "idle_timeout_secs": 300,
                    "idle_check_cmd": ["/usr/local/bin/tikovm-pg-idle-check"],
                    "check_interval_secs": 15
                },
                "block_storage": null
            }
        }])
    }

    /// Parses a full `GET /api/vms` payload produced by the real hostd.
    #[test]
    fn parses_list_response() {
        let value = vm_json();
        let vms: Vec<Vm> = serde_json::from_value(value).unwrap();
        assert_eq!(vms.len(), 1);
        let vm = &vms[0];
        assert_eq!(vm.vm_id, "vm-123-a1b2c3");
        assert_eq!(vm.state, VmState::Started);
        assert_eq!(vm.vm_config.name, "web");
        assert_eq!(vm.vm_config.project_id, 123);
        assert_eq!(vm.vm_config.mode, VmMode::Permanent);
        assert_eq!(vm.vm_config.cpus, 2);
        assert_eq!(vm.guest_ip().unwrap().to_string(), "172.16.115.2");
        assert_eq!(vm.exposed_port_count(), 1);
        assert!(vm.snapshot.is_none());
        assert_eq!(
            vm.vm_config.tags,
            vec!["prod".to_string(), "api".to_string()]
        );
        assert_eq!(
            vm.vm_config
                .auto_suspend
                .as_ref()
                .unwrap()
                .idle_timeout_secs,
            300
        );
    }

    /// Every state value hostd can emit deserializes (no unknown-variant
    /// desync) and prints its `Debug` name.
    #[test]
    fn all_state_variants_parse() {
        for s in [
            "creating",
            "starting",
            "pausing",
            "resuming",
            "suspending",
            "restoring",
            "destroying",
            "created",
            "started",
            "paused",
            "suspended",
            "destroyed",
        ] {
            let state: VmState = serde_json::from_str(&format!("\"{s}\"")).unwrap();
            assert_eq!(state.to_string(), s);
        }
    }

    #[test]
    fn filter_matches_substrings() {
        let vms: Vec<Vm> = serde_json::from_value(vm_json()).unwrap();
        let vm = &vms[0];
        assert!(vm.matches("web"));
        assert!(vm.matches("172.16.115.2"));
        assert!(vm.matches("PROD"));
        assert!(vm.matches("vm-123"));
        assert!(!vm.matches("nope"));
    }
}
