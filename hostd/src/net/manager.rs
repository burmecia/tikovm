//! `NetworkManager`: the public entry point of the `net` module. Ties the
//! pure allocator (`state`) to the host-side effects (`host`) and persists
//! the allocation state after every mutation.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tracing::warn;

use super::cidr::Ipv4Net;
use super::host;
use super::state::{NetState, ProjectSetup};
use super::types::{TapName, VmNet};
use crate::error::{Error, Result};
use crate::vmm::vm::VmId;

pub(crate) struct NetworkManager {
    state: Mutex<NetState>,
    state_path: PathBuf,
    supernet: Ipv4Net,
    subnet_prefix: u8,
}

impl NetworkManager {
    pub(crate) fn new(
        work_dir: impl AsRef<Path>,
        supernet: &str,
        subnet_prefix: u8,
    ) -> Result<Self> {
        let supernet = Ipv4Net::parse(supernet)?;
        if subnet_prefix <= supernet.prefix || subnet_prefix > 30 {
            return Err(Error::net(format!(
                "subnet prefix /{subnet_prefix} must be between /{} and /30 for supernet {supernet}",
                supernet.prefix + 1
            )));
        }

        let state_path = work_dir.as_ref().join("network_state.json");
        let state = match fs::read_to_string(&state_path) {
            Ok(contents) => serde_json::from_str(&contents)
                .map_err(|e| Error::net(format!("parse {}: {e}", state_path.display())))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => NetState::default(),
            Err(e) => return Err(e.into()),
        };

        Ok(Self {
            state: Mutex::new(state),
            state_path,
            supernet,
            subnet_prefix,
        })
    }

    /// Allocate a guest IP (+ TAP, and on first VM of the project the bridge,
    /// gateway, subnet and NAT rule) for `vm_id`.
    pub(crate) fn allocate(
        &self,
        project_id: u64,
        vm_id: &VmId,
        tap_name: &TapName,
    ) -> Result<VmNet> {
        let mut state = self.state.lock()?;
        let plan = state.alloc(
            project_id,
            vm_id,
            tap_name,
            &self.supernet,
            self.subnet_prefix,
        )?;

        let applied = (|| -> Result<()> {
            if let Some(setup) = &plan.setup {
                host::ensure_bridge(setup)?;
            }
            host::ensure_tap(&plan.vm_net.tap_name, &plan.bridge)
        })();

        if let Err(e) = applied {
            let _ = host::delete_tap(&plan.vm_net.tap_name);
            if let Some(setup) = &plan.setup {
                let _ = host::delete_bridge(setup);
            }
            state.rollback_alloc(project_id, vm_id);
            return Err(e);
        }

        persist(&self.state_path, &state)?;
        Ok(plan.vm_net)
    }

    /// Release `vm_id`'s allocation: delete its TAP, free its IP, and when it
    /// was the project's last VM, tear down the bridge + NAT rule and return
    /// the subnet to the pool. Idempotent and best-effort: host-side
    /// failures are logged, not propagated, so `destroy_vm` can always
    /// proceed.
    pub(crate) fn release(&self, vm_id: &VmId) -> Result<()> {
        let mut state = self.state.lock()?;
        let Some(plan) = state.release(vm_id, &self.supernet, self.subnet_prefix) else {
            return Ok(());
        };

        if let Err(e) = host::delete_tap(&plan.tap) {
            warn!(tap = %plan.tap, error = %e, "failed to delete TAP device");
        }
        if let Some(setup) = &plan.teardown
            && let Err(e) = host::delete_bridge(setup)
        {
            warn!(bridge = %setup.bridge, error = %e, "failed to delete bridge");
        }

        persist(&self.state_path, &state)?;
        Ok(())
    }

    /// Delete every device the persisted state claims, plus any stray `tbr-*`
    /// bridge, and reset the state to empty. Safe because no VM (and
    /// therefore no network consumer) survives a hostd restart: Firecracker
    /// children are killed when hostd exits.
    pub(crate) fn reconcile_on_startup(&self) -> Result<()> {
        let mut state = self.state.lock()?;

        for project in state.projects.values() {
            for vm_id in project.vms.values() {
                let tap = TapName::from(&VmId::from(vm_id.as_str()));
                if let Err(e) = host::delete_tap(&tap) {
                    warn!(tap = %tap, error = %e, "reconcile: failed to delete TAP");
                }
            }
            let setup = ProjectSetup {
                bridge: project.bridge.clone(),
                gateway: project.gateway,
                subnet: self
                    .supernet
                    .subnet(project.subnet_index, self.subnet_prefix),
                supernet: self.supernet,
            };
            if let Err(e) = host::delete_bridge(&setup) {
                warn!(bridge = %setup.bridge, error = %e, "reconcile: failed to delete bridge");
            }
        }

        // Stray bridges from a previous run that crashed before persisting.
        for name in host::stray_bridges()? {
            warn!(bridge = %name, "reconcile: deleting stray bridge");
            if let Err(e) = host::run_cmd("ip", &["link", "del", &name]) {
                warn!(bridge = %name, error = %e, "reconcile: failed to delete stray bridge");
            }
        }

        *state = NetState::default();
        persist(&self.state_path, &state)?;
        Ok(())
    }
}

fn persist(path: &Path, state: &NetState) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_string_pretty(state)?)?;
    fs::rename(&tmp, path)?;
    Ok(())
}
