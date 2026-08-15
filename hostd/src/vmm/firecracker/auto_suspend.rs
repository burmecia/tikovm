//! Auto-suspend: suspend idle permanent VMs (snapshot + kill Firecracker)
//! and let later traffic wake them via `ensure_started`.
//!
//! Two detector paths feed `maybe_auto_suspend`:
//! - HTTP: the proxy records per-VM activity (`activity`); a periodic loop
//!   suspends VMs whose exposed ports have been quiet for
//!   `idle_timeout_secs`.
//! - non-HTTP: guestd runs the VM's `idle_check_cmd` and forwards `idle`
//!   events over vsock into `auto_suspend_tx`.
//!
//! Both paths pass through the same gate, so the final decision is always
//! hostd's.

use std::sync::{Arc, Weak};
use std::time::Duration;

use tracing::{debug, info, warn};

use crate::error::Result;
use crate::vmm::Vmm;
use crate::vmm::vm::{VmId, VmMode, VmState};

use super::vmm::FirecrackerVmm;

/// How often the HTTP idle detector scans VMs.
const HTTP_IDLE_POLL: Duration = Duration::from_secs(10);

impl FirecrackerVmm {
    /// Spawn the background loops (auto-suspend detectors and the cron
    /// scheduler). Must be called once after the VMM is wrapped in an `Arc`.
    pub(crate) fn start_background_tasks(self: &Arc<Self>) {
        *self.self_ref.lock().unwrap() = Some(Arc::downgrade(self));

        if let Some(mut rx) = self.auto_suspend_rx.lock().unwrap().take() {
            let vmm = Arc::clone(self);
            tokio::spawn(async move {
                while let Some(vm_id) = rx.recv().await {
                    vmm.maybe_auto_suspend(&vm_id).await;
                }
            });
        }

        let vmm = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(HTTP_IDLE_POLL);
            loop {
                interval.tick().await;
                vmm.check_http_idle_vms().await;
            }
        });

        self.spawn_scheduler();
    }

    /// HTTP idle detector: suspend VMs with exposed ports that have seen no
    /// proxied request for `idle_timeout_secs`.
    async fn check_http_idle_vms(&self) {
        let vm_ids: Vec<VmId> = match self.vms.lock() {
            Ok(vms) => vms.keys().cloned().collect(),
            Err(_) => return,
        };
        for vm_id in vm_ids {
            match self.http_idle_expired(&vm_id) {
                Ok(true) => self.maybe_auto_suspend(&vm_id).await,
                Ok(false) => {}
                Err(e) => warn!(vm_id = %vm_id, error = %e, "auto-suspend idle check failed"),
            }
        }
    }

    /// Whether the VM's HTTP idle timer has expired: it has exposed ports
    /// and neither a proxied request nor a wake happened within
    /// `idle_timeout_secs`.
    fn http_idle_expired(&self, vm_id: &VmId) -> Result<bool> {
        let idle_timeout = {
            let vms = self.vms.lock()?;
            let Some(entry) = vms.get(vm_id) else {
                return Ok(false);
            };
            let instance = entry.instance.lock()?;
            let Some(config) = &instance.vm_config.auto_suspend else {
                return Ok(false);
            };
            if instance.vm_config.network_config.exposed_ports.is_empty() {
                return Ok(false); // no HTTP exposure: HTTP detector inert
            }
            config.idle_timeout_secs
        };

        let last_active = [
            self.activity.last_activity(vm_id),
            self.wake_times.lock()?.get(vm_id).copied(),
        ]
        .into_iter()
        .flatten()
        .max();
        let Some(last_active) = last_active else {
            return Ok(false); // never started
        };
        Ok(last_active.elapsed() >= Duration::from_secs(idle_timeout))
    }

    /// Proactively connect to guestd so its idle detector gets configured
    /// (`install_guest_conn` pushes the config on connect). Needed after
    /// start/restore: for a VM whose only detector is `idle_check_cmd`, no
    /// workload may ever start, and without a connection the detector would
    /// never be armed (and its `idle` events would have nowhere to go).
    pub(super) fn arm_guest_detector(&self, vm_id: &VmId) {
        let needs_arm = {
            let Ok(vms) = self.vms.lock() else { return };
            let Some(entry) = vms.get(vm_id) else { return };
            let Ok(instance) = entry.instance.lock() else {
                return;
            };
            instance
                .vm_config
                .auto_suspend
                .as_ref()
                .is_some_and(|c| !c.idle_check_cmd.is_empty())
        };
        if !needs_arm {
            return;
        }
        let vmm = self
            .self_ref
            .lock()
            .unwrap()
            .as_ref()
            .and_then(Weak::upgrade);
        let Some(vmm) = vmm else { return };
        let vm_id = vm_id.clone();
        tokio::spawn(async move {
            // guest_conn retries for a minute while the guest boots.
            if let Err(e) = vmm.guest_conn(&vm_id).await {
                warn!(vm_id = %vm_id, error = %e, "failed to arm guest idle detector");
            }
        });
    }

    /// Suspend `vm_id` if the gate allows it. Triggered by guest `idle`
    /// events and by the HTTP idle loop; quietly does nothing otherwise.
    async fn maybe_auto_suspend(&self, vm_id: &VmId) {
        match self.auto_suspend_gate(vm_id) {
            Ok(true) => {
                info!(vm_id = %vm_id, "auto-suspending idle VM");
                if let Err(e) = self.snapshot_vm(vm_id).await {
                    warn!(vm_id = %vm_id, error = %e, "auto-suspend snapshot failed");
                }
            }
            Ok(false) => debug!(vm_id = %vm_id, "auto-suspend gated"),
            Err(e) => warn!(vm_id = %vm_id, error = %e, "auto-suspend gate failed"),
        }
    }

    /// The gate every auto-suspend trigger passes through: the VM must be a
    /// started permanent VM with an `auto_suspend` config, have no in-flight
    /// proxied requests and no active workloads (snapshotting mid-workload
    /// would kill the guest process and strand the workload), and be past
    /// the post-wake cooldown (so a restored VM cannot flap straight back
    /// down).
    fn auto_suspend_gate(&self, vm_id: &VmId) -> Result<bool> {
        let idle_timeout = {
            let vms = self.vms.lock()?;
            let Some(entry) = vms.get(vm_id) else {
                return Ok(false);
            };
            if entry.workloads.values().any(|w| w.is_active()) {
                return Ok(false);
            }
            let instance = entry.instance.lock()?;
            if instance.state != VmState::Started || instance.vm_config.mode != VmMode::Permanent {
                return Ok(false);
            }
            match &instance.vm_config.auto_suspend {
                Some(config) => config.idle_timeout_secs,
                None => return Ok(false),
            }
        };

        if self.activity.in_flight(vm_id) > 0 {
            return Ok(false);
        }
        if let Some(woken) = self.wake_times.lock()?.get(vm_id)
            && woken.elapsed() < Duration::from_secs(idle_timeout)
        {
            return Ok(false);
        }
        Ok(true)
    }
}
