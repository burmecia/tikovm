//! The `FirecrackerVmm`: an implementation of the `Vmm` trait driving
//! Firecracker microVMs via their API socket and a vsock channel to guestd.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tracing::{debug, info, warn};

use crate::error::{Error, Result};
use crate::net::{NetworkManager, TapName};
use crate::vmm::Vmm;
use crate::vmm::vm::{VmConfig, VmId, VmInstance, VmInstanceRef, VmSnapshot, VmState};
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
}

impl FirecrackerVmm {
    pub(crate) fn new(
        assets_dir: impl AsRef<Path>,
        work_dir: impl AsRef<Path>,
        net_mgr: Arc<NetworkManager>,
    ) -> Result<Self> {
        let fc_bin = from_path_or_env("firecracker", ENV_FIRECRACKER_BIN);
        debug!(?fc_bin, "Firecracker binary");
        Ok(Self {
            fc_bin,
            assets_dir: assets_dir.as_ref().to_path_buf(),
            work_dir: work_dir.as_ref().to_path_buf(),
            net_mgr,
            vms: Arc::new(Mutex::new(HashMap::new())),
            next_cid: AtomicU32::new(3),
        })
    }
}

#[async_trait]
impl Vmm for FirecrackerVmm {
    async fn create_vm(&self, config: &VmConfig) -> Result<VmId> {
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

        info!(vm_id = %vm_id, "VM destroyed");

        Ok(())
    }
}
