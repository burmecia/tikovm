//! The `FirecrackerVmm`: an implementation of the `Vmm` trait driving
//! Firecracker microVMs via their API socket and a vsock channel to guestd.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::error::{Error, Result};
use crate::net::{ExposedPort, NetworkManager, TapName};
use crate::vmm::Vmm;
use crate::vmm::activity::ActivityTracker;
use crate::vmm::vm::{VmConfig, VmId, VmInstance, VmInstanceRef, VmMode, VmSnapshot, VmState};
use crate::vmm::workload::{Workload, WorkloadId, WorkloadLogEntry, WorkloadSpec};

use super::api::FcApiClient;
use super::setup::from_path_or_env;
use super::vsock::{GuestConnHandle, GuestRequest};

const ENV_FIRECRACKER_BIN: &str = "FIRECRACKER_BIN";

pub(super) struct FcVmEntry {
    pub(super) instance: VmInstanceRef,

    /// Firecracker API client for this VM.
    api_client: Arc<FcApiClient>,

    /// Firecracker child process.
    fc: Option<tokio::process::Child>,

    /// Workloads running (or finished) in this VM via guestd.
    pub(super) workloads: HashMap<WorkloadId, Workload>,

    /// Live vsock control connection to guestd, if any.
    pub(super) guest_conn: Option<GuestConnHandle>,

    /// Directory holding per-workload log files (`<workload_id>.log`, JSON lines).
    pub(super) workloads_dir: PathBuf,
}

impl FcVmEntry {
    fn new(instance: VmInstanceRef) -> Self {
        let (socket_path, workloads_dir) = {
            let instance = instance.lock().unwrap();
            (
                instance.socket_path.clone(),
                instance.work_dir.join("workloads"),
            )
        };
        let api_client = Arc::new(FcApiClient::new(socket_path));
        Self {
            instance,
            api_client,
            fc: None,
            workloads: HashMap::new(),
            guest_conn: None,
            workloads_dir,
        }
    }
}

pub(crate) struct FirecrackerVmm {
    pub(super) fc_bin: PathBuf,
    assets_dir: PathBuf,
    work_dir: PathBuf,
    net_mgr: Arc<NetworkManager>,
    pub(super) vms: Arc<Mutex<HashMap<VmId, FcVmEntry>>>,
    /// Next vsock guest CID to hand out (0, 1 and 2 = host are reserved).
    next_cid: AtomicU32,

    /// Proxy activity per VM (auto-suspend input).
    activity: ActivityTracker,

    /// Guest `idle` events arrive here (from the vsock connection tasks) and
    /// are drained by the auto-suspend background loop.
    pub(super) auto_suspend_tx: mpsc::Sender<VmId>,
    /// Receiving half, taken by `start_background_tasks`.
    auto_suspend_rx: Mutex<Option<mpsc::Receiver<VmId>>>,

    /// Per-VM locks deduplicating concurrent restores (proxy/exec wake-ups).
    wake_locks: Mutex<HashMap<VmId, Arc<tokio::sync::Mutex<()>>>>,
    /// When each VM last reached `Started` (start or restore); the
    /// auto-suspend cooldown and HTTP idle timer are measured from here.
    wake_times: Mutex<HashMap<VmId, Instant>>,

    /// Weak self-reference (set by `start_background_tasks`) so lifecycle
    /// methods can spawn tasks that need `Arc<Self>`.
    self_ref: Mutex<Option<Weak<FirecrackerVmm>>>,
}

impl FirecrackerVmm {
    pub(crate) fn new(
        assets_dir: impl AsRef<Path>,
        work_dir: impl AsRef<Path>,
        net_mgr: Arc<NetworkManager>,
    ) -> Result<Self> {
        let fc_bin = from_path_or_env("firecracker", ENV_FIRECRACKER_BIN);
        debug!(?fc_bin, "Firecracker binary");
        let (auto_suspend_tx, auto_suspend_rx) = mpsc::channel(64);
        Ok(Self {
            fc_bin,
            assets_dir: assets_dir.as_ref().to_path_buf(),
            work_dir: work_dir.as_ref().to_path_buf(),
            net_mgr,
            vms: Arc::new(Mutex::new(HashMap::new())),
            next_cid: AtomicU32::new(3),
            activity: ActivityTracker::new(),
            auto_suspend_tx,
            auto_suspend_rx: Mutex::new(Some(auto_suspend_rx)),
            wake_locks: Mutex::new(HashMap::new()),
            wake_times: Mutex::new(HashMap::new()),
            self_ref: Mutex::new(None),
        })
    }
}

/// Auto-suspend: suspend idle permanent VMs (snapshot + kill Firecracker)
/// and let later traffic wake them via `ensure_started`.
///
/// Two detector paths feed `maybe_auto_suspend`:
/// - HTTP: the proxy records per-VM activity (`activity`); a periodic loop
///   suspends VMs whose exposed ports have been quiet for
///   `idle_timeout_secs`.
/// - non-HTTP: guestd runs the VM's `idle_check_cmd` and forwards `idle`
///   events over vsock into `auto_suspend_tx`.
///
/// Both paths pass through the same gate, so the final decision is always
/// hostd's.
impl FirecrackerVmm {
    /// Spawn the auto-suspend background loops. Must be called once after
    /// the VMM is wrapped in an `Arc`.
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
    fn arm_guest_detector(&self, vm_id: &VmId) {
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
    /// started permanent VM with an auto_suspend config, have no in-flight
    /// proxied requests, and be past the post-wake cooldown (so a restored
    /// VM cannot flap straight back down).
    fn auto_suspend_gate(&self, vm_id: &VmId) -> Result<bool> {
        let idle_timeout = {
            let vms = self.vms.lock()?;
            let Some(entry) = vms.get(vm_id) else {
                return Ok(false);
            };
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

/// How often the HTTP idle detector scans VMs.
const HTTP_IDLE_POLL: Duration = Duration::from_secs(10);

#[async_trait]
impl Vmm for FirecrackerVmm {
    async fn create_vm(&self, config: &VmConfig) -> Result<VmId> {
        if config.auto_suspend.is_some() && config.mode != VmMode::Permanent {
            return Err(Error::vmm(
                "auto_suspend is only supported for permanent VMs",
            ));
        }

        let mut instance = VmInstance::new(config, &self.assets_dir, &self.work_dir)?;
        let vm_id = instance.vm_id.clone();
        fs::create_dir_all(&instance.work_dir)?;

        // Allocate networking (TAP + guest IP, and on the project's first VM
        // its bridge + subnet) up front, so a failure below can roll it back.
        let vm_net = self
            .net_mgr
            .allocate(config.project_id, &vm_id, &TapName::from(&vm_id))?;
        instance.net = Some(vm_net);
        instance.guest_cid = Some(self.next_cid.fetch_add(1, Ordering::Relaxed));
        let instance_ref = instance.into_ref();

        // Spawn and configure Firecracker before registering the VM, so a
        // failure leaves nothing behind in the map.
        let setup = async {
            let child = self.spawn_fc_process(instance_ref.clone())?;
            self.configure_vm(instance_ref.clone()).await?;
            Ok::<_, Error>(child)
        }
        .await;

        let child = match setup {
            Ok(child) => child,
            Err(e) => {
                if let Err(rel_err) = self.net_mgr.release(&vm_id) {
                    warn!(vm_id = %vm_id, error = %rel_err, "failed to roll back network allocation");
                }
                if let Ok(instance) = instance_ref.lock() {
                    let _ = instance.cleanup_runtime_artifacts();
                }
                return Err(e);
            }
        };

        self.vms
            .lock()?
            .insert(vm_id.clone(), FcVmEntry::new(instance_ref.clone()));

        let mut vms = self.vms.lock()?;
        let entry = vms
            .get_mut(&vm_id)
            .ok_or_else(|| Error::VmNotFound(vm_id.to_string()))?;
        entry.fc = Some(child);
        instance_ref.lock()?.state.transition(VmState::Created)?;

        Ok(vm_id)
    }

    async fn get_vm(&self, vm_id: &VmId) -> Result<Option<VmInstanceRef>> {
        Ok(self
            .vms
            .lock()?
            .get(vm_id)
            .map(|entry| entry.instance.clone()))
    }

    async fn list_vms(&self) -> Result<Vec<VmInstanceRef>> {
        Ok(self
            .vms
            .lock()?
            .values()
            .map(|entry| entry.instance.clone())
            .collect())
    }

    async fn start_vm(&self, vm_id: &VmId) -> Result<()> {
        let (instance_ref, client) = {
            let vms = self.vms.lock()?;
            let entry = vms
                .get(vm_id)
                .ok_or_else(|| Error::VmNotFound(vm_id.to_string()))?;
            (entry.instance.clone(), entry.api_client.clone())
        };

        instance_ref.lock()?.state.transition(VmState::Starting)?;

        // Start the VM via the Firecracker API.
        match client
            .put("/actions", &json!({"action_type": "InstanceStart"}))
            .await
        {
            Ok(_) => {
                // Make sure the VM actually finished starting before
                // reporting it as ready to use.
                match client.wait_until_running(Duration::from_secs(5)).await {
                    Ok(_) => {
                        instance_ref.lock()?.state.transition(VmState::Started)?;
                        self.wake_times.lock()?.insert(vm_id.clone(), Instant::now());
                        self.arm_guest_detector(vm_id);
                    }
                    Err(e) => {
                        instance_ref.lock()?.state.transition(VmState::Created)?;
                        return Err(e);
                    }
                }
            }
            Err(e) => {
                instance_ref.lock()?.state.transition(VmState::Created)?;
                return Err(e);
            }
        }

        Ok(())
    }

    async fn pause_vm(&self, vm_id: &VmId) -> Result<()> {
        let (instance_ref, client) = {
            let vms = self.vms.lock()?;
            let entry = vms
                .get(vm_id)
                .ok_or_else(|| Error::VmNotFound(vm_id.to_string()))?;
            (entry.instance.clone(), entry.api_client.clone())
        };

        instance_ref.lock()?.state.transition(VmState::Pausing)?;

        // Pause the VM via the Firecracker API, rolling back to Started on
        // failure.
        match client.patch("/vm", &json!({"state": "Paused"})).await {
            Ok(_) => {
                instance_ref.lock()?.state.transition(VmState::Paused)?;
            }
            Err(e) => {
                instance_ref.lock()?.state.transition(VmState::Started)?;
                return Err(e);
            }
        }

        Ok(())
    }

    async fn resume_vm(&self, vm_id: &VmId) -> Result<()> {
        let (instance_ref, client) = {
            let vms = self.vms.lock()?;
            let entry = vms
                .get(vm_id)
                .ok_or_else(|| Error::VmNotFound(vm_id.to_string()))?;
            (entry.instance.clone(), entry.api_client.clone())
        };

        instance_ref.lock()?.state.transition(VmState::Resuming)?;

        // Resume the VM via the Firecracker API, rolling back to Paused on
        // failure.
        match client.patch("/vm", &json!({"state": "Resumed"})).await {
            Ok(_) => {
                instance_ref.lock()?.state.transition(VmState::Started)?;
            }
            Err(e) => {
                instance_ref.lock()?.state.transition(VmState::Paused)?;
                return Err(e);
            }
        }

        Ok(())
    }

    async fn snapshot_vm(&self, vm_id: &VmId) -> Result<VmSnapshot> {
        let (instance_ref, client) = {
            let vms = self.vms.lock()?;
            let entry = vms
                .get(vm_id)
                .ok_or_else(|| Error::VmNotFound(vm_id.to_string()))?;
            (entry.instance.clone(), entry.api_client.clone())
        };

        instance_ref.lock()?.state.transition(VmState::Suspending)?;

        let result = async {
            // Firecracker requires the VM to be paused before taking a
            // snapshot.
            client.patch("/vm", &json!({"state": "Paused"})).await?;

            let snapshot = instance_ref.lock()?.new_snapshot();
            client
                .put(
                    "/snapshot/create",
                    &json!({
                        "snapshot_type": "Full",
                        "snapshot_path": snapshot.state_path.to_string_lossy(),
                        "mem_file_path": snapshot.mem_path.to_string_lossy(),
                    }),
                )
                .await?;

            Ok::<_, Error>(snapshot)
        }
        .await;

        match result {
            Ok(snapshot) => {
                // Stop the Firecracker process so a suspended VM consumes no
                // resources; the snapshot files on disk are what the
                // Suspended state rests on. Restore spawns a fresh process.
                let old_child = self
                    .vms
                    .lock()?
                    .get_mut(vm_id)
                    .and_then(|entry| entry.fc.take());
                if let Some(mut child) = old_child
                    && let Err(e) = child.kill().await
                {
                    warn!(vm_id = %vm_id, error = %e, "failed to kill Firecracker process after snapshotting");
                }

                let mut instance = instance_ref.lock()?;
                let _ = fs::remove_file(&instance.socket_path);
                instance.snapshot = Some(snapshot.clone());
                instance.state.transition(VmState::Suspended)?;
                info!(vm_id = %vm_id, "VM snapshot created");
                Ok(snapshot)
            }
            Err(e) => {
                // Best effort: bring the VM back to Running so the rollback
                // to Started reflects reality.
                let _ = client.patch("/vm", &json!({"state": "Resumed"})).await;
                instance_ref.lock()?.state.transition(VmState::Started)?;
                Err(e)
            }
        }
    }

    async fn restore_vm(&self, vm_id: &VmId) -> Result<()> {
        let (instance_ref, client, snapshot) = {
            let vms = self.vms.lock()?;
            let entry = vms
                .get(vm_id)
                .ok_or_else(|| Error::VmNotFound(vm_id.to_string()))?;
            let snapshot = entry
                .instance
                .lock()?
                .snapshot
                .clone()
                .ok_or_else(|| Error::vmm(format!("vm {vm_id} has no snapshot")))?;
            (entry.instance.clone(), entry.api_client.clone(), snapshot)
        };

        instance_ref.lock()?.state.transition(VmState::Restoring)?;

        let result = async {
            let child = self.spawn_fc_process(instance_ref.clone())?;

            // Load the snapshot and resume the VM in one go.
            client
                .put(
                    "/snapshot/load",
                    &json!({
                        "snapshot_path": snapshot.state_path.to_string_lossy(),
                        "mem_file_path": snapshot.mem_path.to_string_lossy(),
                        "enable_diff_snapshots": false,
                        "resume_vm": true,
                    }),
                )
                .await?;
            client.wait_until_running(Duration::from_secs(5)).await?;

            Ok::<_, Error>(child)
        }
        .await;

        match result {
            Ok(child) => {
                self.vms
                    .lock()?
                    .get_mut(vm_id)
                    .ok_or_else(|| Error::VmNotFound(vm_id.to_string()))?
                    .fc = Some(child);
                instance_ref.lock()?.state.transition(VmState::Started)?;
                self.wake_times.lock()?.insert(vm_id.clone(), Instant::now());
                self.arm_guest_detector(vm_id);
                info!(vm_id = %vm_id, "VM restored from snapshot");
                Ok(())
            }
            Err(e) => {
                // The snapshot files are still on disk, so the VM stays in
                // Suspended and the restore can be retried.
                instance_ref.lock()?.state.transition(VmState::Suspended)?;
                Err(e)
            }
        }
    }

    /// Start a workload in a started VM: send guestd the command and track
    /// the resulting process's lifecycle and output.
    async fn start_workload(&self, vm_id: &VmId, spec: WorkloadSpec) -> Result<Workload> {
        if spec.cmd.is_empty() {
            return Err(Error::vmm("workload cmd must not be empty"));
        }

        {
            let vms = self.vms.lock()?;
            let entry = vms
                .get(vm_id)
                .ok_or_else(|| Error::VmNotFound(vm_id.to_string()))?;
            let state = entry.instance.lock()?.state;
            if state != VmState::Started {
                return Err(Error::vmm(format!(
                    "vm {vm_id} is {state:?}; workloads require a started vm"
                )));
            }
            fs::create_dir_all(&entry.workloads_dir)?;
        }

        let workload = Workload::new(vm_id, spec);
        // Connecting can take a while on a freshly booted guest (see
        // guest_conn), so it happens outside the lock.
        let conn = self.guest_conn(vm_id).await?;

        // Register before sending so an instantly-arriving started event
        // finds the workload in the map.
        {
            let mut vms = self.vms.lock()?;
            let entry = vms
                .get_mut(vm_id)
                .ok_or_else(|| Error::VmNotFound(vm_id.to_string()))?;
            entry
                .workloads
                .insert(workload.workload_id.clone(), workload.clone());
        }

        let request = GuestRequest::Start {
            workload_id: workload.workload_id.0.clone(),
            cmd: workload.spec.cmd.clone(),
            env: workload
                .spec
                .env
                .iter()
                .map(|e| (e.key.clone(), e.value.clone()))
                .collect(),
            cwd: workload.spec.cwd.clone(),
        };
        if let Err(e) = conn.send(request).await {
            if let Ok(mut vms) = self.vms.lock()
                && let Some(entry) = vms.get_mut(vm_id)
            {
                entry.workloads.remove(&workload.workload_id);
            }
            return Err(e);
        }

        Ok(workload)
    }

    /// Ask guestd to stop a running workload (SIGTERM, then SIGKILL after a
    /// grace period in the guest). The workload lands in `stopped` when the
    /// exit event arrives.
    async fn stop_workload(&self, vm_id: &VmId, workload_id: &WorkloadId) -> Result<Workload> {
        let conn = self.guest_conn(vm_id).await?;
        {
            let mut vms = self.vms.lock()?;
            let entry = vms
                .get_mut(vm_id)
                .ok_or_else(|| Error::VmNotFound(vm_id.to_string()))?;
            let workload = entry
                .workloads
                .get_mut(workload_id)
                .ok_or_else(|| Error::WorkloadNotFound(workload_id.to_string()))?;
            if !workload.is_active() {
                return Err(Error::vmm(format!(
                    "workload {workload_id} is {:?}; only active workloads can be stopped",
                    workload.state
                )));
            }
            workload.stop_requested = true;
        }
        if let Err(e) = conn
            .send(GuestRequest::Stop {
                workload_id: workload_id.0.clone(),
            })
            .await
        {
            if let Ok(mut vms) = self.vms.lock()
                && let Some(entry) = vms.get_mut(vm_id)
                && let Some(workload) = entry.workloads.get_mut(workload_id)
            {
                workload.stop_requested = false;
            }
            return Err(e);
        }
        self.get_workload(vm_id, workload_id).await
    }

    async fn list_workloads(&self, vm_id: &VmId) -> Result<Vec<Workload>> {
        let vms = self.vms.lock()?;
        let entry = vms
            .get(vm_id)
            .ok_or_else(|| Error::VmNotFound(vm_id.to_string()))?;
        Ok(entry.workloads.values().cloned().collect())
    }

    async fn get_workload(&self, vm_id: &VmId, workload_id: &WorkloadId) -> Result<Workload> {
        let vms = self.vms.lock()?;
        let entry = vms
            .get(vm_id)
            .ok_or_else(|| Error::VmNotFound(vm_id.to_string()))?;
        entry
            .workloads
            .get(workload_id)
            .cloned()
            .ok_or_else(|| Error::WorkloadNotFound(workload_id.to_string()))
    }

    /// Exposed ports of a VM (the VM-side registry in `network_config`).
    async fn list_exposed_ports(&self, vm_id: &VmId) -> Result<Vec<ExposedPort>> {
        let vms = self.vms.lock()?;
        let entry = vms
            .get(vm_id)
            .ok_or_else(|| Error::VmNotFound(vm_id.to_string()))?;
        let instance = entry.instance.lock()?;
        Ok(instance.vm_config.network_config.exposed_ports.clone())
    }

    /// Register an exposed port on a VM.
    async fn add_exposed_port(&self, vm_id: &VmId, port: ExposedPort) -> Result<ExposedPort> {
        let vms = self.vms.lock()?;
        let entry = vms
            .get(vm_id)
            .ok_or_else(|| Error::VmNotFound(vm_id.to_string()))?;
        let mut instance = entry.instance.lock()?;
        instance
            .vm_config
            .network_config
            .add_exposed_port(vm_id, port.clone())?;
        Ok(port)
    }

    /// Remove an exposed port from a VM by port number.
    async fn remove_exposed_port(&self, vm_id: &VmId, port: u16) -> Result<()> {
        let vms = self.vms.lock()?;
        let entry = vms
            .get(vm_id)
            .ok_or_else(|| Error::VmNotFound(vm_id.to_string()))?;
        let mut instance = entry.instance.lock()?;
        instance
            .vm_config
            .network_config
            .remove_exposed_port(vm_id, port)
    }

    /// Captured stdout/stderr of a workload, in arrival order.
    async fn workload_logs(
        &self,
        vm_id: &VmId,
        workload_id: &WorkloadId,
    ) -> Result<Vec<WorkloadLogEntry>> {
        let log_path = {
            let vms = self.vms.lock()?;
            let entry = vms
                .get(vm_id)
                .ok_or_else(|| Error::VmNotFound(vm_id.to_string()))?;
            if !entry.workloads.contains_key(workload_id) {
                return Err(Error::WorkloadNotFound(workload_id.to_string()));
            }
            workload_id.log_path(&entry.workloads_dir)
        };
        let contents = match fs::read_to_string(&log_path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        Ok(contents
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect())
    }

    async fn destroy_vm(&self, vm_id: &VmId) -> Result<()> {
        let mut entry = self
            .vms
            .lock()?
            .remove(vm_id)
            .ok_or_else(|| Error::VmNotFound(vm_id.to_string()))?;

        // Kill the Firecracker process. An error here just means the process
        // already exited, so keep going with cleanup.
        if let Some(mut child) = entry.fc.take()
            && let Err(e) = child.kill().await
        {
            debug!(vm_id = %vm_id, error = %e, "failed to kill Firecracker process");
        }

        // Release the network allocation (TAP + guest IP; bridge + subnet
        // when this was the project's last VM). Best-effort: host-side
        // failures are logged by release, not propagated.
        if let Err(e) = self.net_mgr.release(vm_id) {
            warn!(vm_id = %vm_id, error = %e, "failed to release network allocation");
        }

        // Clean up runtime artifacts.
        entry.instance.lock()?.cleanup_runtime_artifacts()?;

        // Drop the auto-suspend bookkeeping for this VM.
        self.wake_times.lock()?.remove(vm_id);
        self.wake_locks.lock()?.remove(vm_id);
        self.activity.clear(vm_id);

        info!(vm_id = %vm_id, "VM destroyed");

        Ok(())
    }

    async fn ensure_started(&self, vm_id: &VmId) -> Result<()> {
        // Per-VM lock: concurrent wake-ups (proxy requests, exec) share one
        // restore, and a second caller arriving mid-restore just waits for
        // the first to finish and finds the VM Started.
        let wake_lock = {
            let mut locks = self.wake_locks.lock()?;
            locks
                .entry(vm_id.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        let _guard = wake_lock.lock().await;

        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let state = {
                let vms = self.vms.lock()?;
                let entry = vms
                    .get(vm_id)
                    .ok_or_else(|| Error::VmNotFound(vm_id.to_string()))?;
                entry.instance.lock()?.state
            };
            match state {
                VmState::Started => return Ok(()),
                VmState::Suspended => {
                    info!(vm_id = %vm_id, "waking suspended VM");
                    // restore_vm records the wake time on success.
                    return self.restore_vm(vm_id).await;
                }
                // A snapshot or restore triggered outside this path (the
                // management API) is in flight; wait for it to settle.
                VmState::Suspending | VmState::Restoring => {
                    if Instant::now() >= deadline {
                        return Err(Error::vmm(format!(
                            "vm {vm_id} is still {state:?} after 30s"
                        )));
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                _ => {
                    return Err(Error::vmm(format!(
                        "vm {vm_id} is {state:?}; only a suspended vm can be woken"
                    )));
                }
            }
        }
    }

    fn activity(&self) -> ActivityTracker {
        self.activity.clone()
    }
}
