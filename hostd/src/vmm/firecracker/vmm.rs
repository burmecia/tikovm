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
use crate::storage::{ALLOWED_CHUNK_KB, StorageManager};
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
    storage_mgr: Arc<StorageManager>,
    pub(super) vms: Arc<Mutex<HashMap<VmId, FcVmEntry>>>,
    /// Next vsock guest CID to hand out (0, 1 and 2 = host are reserved).
    next_cid: AtomicU32,

    /// Proxy activity per VM (auto-suspend input).
    pub(super) activity: ActivityTracker,

    /// Guest `idle` events arrive here (from the vsock connection tasks) and
    /// are drained by the auto-suspend background loop.
    pub(super) auto_suspend_tx: mpsc::Sender<VmId>,
    /// Receiving half, taken by `start_background_tasks`.
    pub(super) auto_suspend_rx: Mutex<Option<mpsc::Receiver<VmId>>>,

    /// Per-VM locks deduplicating concurrent restores (proxy/exec wake-ups).
    wake_locks: Mutex<HashMap<VmId, Arc<tokio::sync::Mutex<()>>>>,
    /// When each VM last reached `Started` (start or restore); the
    /// auto-suspend cooldown and HTTP idle timer are measured from here.
    pub(super) wake_times: Mutex<HashMap<VmId, Instant>>,

    /// Per-VM locks deduplicating scheduled runs (see `schedule` module).
    pub(super) schedule_run_locks: super::schedule::ScheduleRunLocks,

    /// Weak self-reference (set by `start_background_tasks`) so lifecycle
    /// methods can spawn tasks that need `Arc<Self>`.
    pub(super) self_ref: Mutex<Option<Weak<FirecrackerVmm>>>,
}

impl FirecrackerVmm {
    pub(crate) fn new(
        assets_dir: impl AsRef<Path>,
        work_dir: impl AsRef<Path>,
        net_mgr: Arc<NetworkManager>,
        storage_mgr: Arc<StorageManager>,
    ) -> Self {
        let fc_bin = from_path_or_env("firecracker", ENV_FIRECRACKER_BIN);
        debug!(?fc_bin, "Firecracker binary");
        let (auto_suspend_tx, auto_suspend_rx) = mpsc::channel(64);
        Self {
            fc_bin,
            assets_dir: assets_dir.as_ref().to_path_buf(),
            work_dir: work_dir.as_ref().to_path_buf(),
            net_mgr,
            storage_mgr,
            vms: Arc::new(Mutex::new(HashMap::new())),
            next_cid: AtomicU32::new(3),
            activity: ActivityTracker::new(),
            auto_suspend_tx,
            auto_suspend_rx: Mutex::new(Some(auto_suspend_rx)),
            wake_locks: Mutex::new(HashMap::new()),
            wake_times: Mutex::new(HashMap::new()),
            schedule_run_locks: super::schedule::ScheduleRunLocks::default(),
            self_ref: Mutex::new(None),
        }
    }

    /// Fetch a VM's instance and Firecracker API client in one lock scope —
    /// the shared prologue of the lifecycle operations below.
    fn entry_parts(&self, vm_id: &VmId) -> Result<(VmInstanceRef, Arc<FcApiClient>)> {
        let vms = self.vms.lock()?;
        let entry = vms
            .get(vm_id)
            .ok_or_else(|| Error::VmNotFound(vm_id.to_string()))?;
        Ok((entry.instance.clone(), entry.api_client.clone()))
    }
}

#[async_trait]
impl Vmm for FirecrackerVmm {
    async fn create_vm(&self, config: &VmConfig) -> Result<VmId> {
        if config.auto_suspend.is_some() && config.mode != VmMode::Permanent {
            return Err(Error::vmm(
                "auto_suspend is only supported for permanent VMs",
            ));
        }

        // Schedule-mode validation: the scheduler needs a cmd to run and a
        // parseable cron expression; the schedule-only fields are rejected
        // for other modes so a typo can't silently misconfigure a VM.
        if config.mode == VmMode::Schedule {
            if config.cmd.is_empty() {
                return Err(Error::vmm("schedule VMs require a non-empty cmd"));
            }
            let Some(cron_schedule) = &config.cron_schedule else {
                return Err(Error::vmm("schedule VMs require a cron_schedule"));
            };
            super::schedule::parse_cron_schedule(cron_schedule)?;
        } else if config.cron_schedule.is_some() || config.timeout_secs.is_some() {
            return Err(Error::vmm(
                "cron_schedule and timeout_secs are only supported for schedule VMs",
            ));
        }

        // Block-storage validation (see `storage` module for semantics).
        if let Some(bs) = &config.block_storage {
            if bs.size_mb < 128 {
                return Err(Error::vmm("block_storage size_mb must be >= 128"));
            }
            if let Some(chunk_kb) = bs.chunk_kb
                && !ALLOWED_CHUNK_KB.contains(&chunk_kb)
            {
                return Err(Error::vmm(format!(
                    "block_storage chunk_kb must be one of {ALLOWED_CHUNK_KB:?}"
                )));
            }
            if !bs.mount_path.is_empty() {
                let ok = bs.mount_path.starts_with('/')
                    && bs.mount_path.len() > 1
                    && bs
                        .mount_path
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.'));
                if !ok {
                    return Err(Error::vmm(
                        "block_storage mount_path must be an absolute path of [a-z0-9/_-.]",
                    ));
                }
            }
        }

        // Image defaults: a postgres-16 VM with an auto_suspend config but
        // no explicit idle_check_cmd gets the SQL-based idle check its
        // image ships (scripts/rootfs/build_rootfs_postgres16.sh), so both
        // detector paths (proxy activity + guest idle events) work without
        // the caller naming the script. An explicit command always wins.
        let mut config = config.clone();
        if config.image == "postgres-16"
            && let Some(auto_suspend) = &mut config.auto_suspend
            && auto_suspend.idle_check_cmd.is_empty()
        {
            auto_suspend.idle_check_cmd = vec!["/usr/local/bin/tikovm-pg-idle-check".to_string()];
        }
        let config = &config;

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
            // Attach the optional block volume before boot so the drive
            // configuration can reference its device path (/dev/vdc).
            if let Some(bs) = &config.block_storage {
                let dev = self
                    .storage_mgr
                    .attach(&vm_id, config.project_id, bs)
                    .await?;
                instance_ref.lock()?.block_device = Some(dev);
            }
            let child = self.spawn_fc_process(&instance_ref).await?;
            self.configure_vm(instance_ref.clone()).await?;
            Ok::<_, Error>(child)
        }
        .await;

        let child = match setup {
            Ok(child) => child,
            Err(e) => {
                if instance_ref
                    .lock()
                    .map(|i| i.block_device.is_some())
                    .unwrap_or(false)
                {
                    self.storage_mgr.detach(&vm_id).await;
                }
                if let Err(rel_err) = self.net_mgr.release(&vm_id) {
                    warn!(vm_id = %vm_id, error = %rel_err, "failed to roll back network allocation");
                }
                if let Ok(instance) = instance_ref.lock() {
                    instance.cleanup_runtime_artifacts();
                }
                return Err(e);
            }
        };

        let mut vms = self.vms.lock()?;
        vms.insert(vm_id.clone(), FcVmEntry::new(instance_ref.clone()));
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
        let (instance_ref, client) = self.entry_parts(vm_id)?;

        instance_ref.lock()?.state.transition(VmState::Starting)?;

        // Start the VM via the Firecracker API.
        match client
            .put("/actions", &json!({"action_type": "InstanceStart"}))
            .await
        {
            Ok(()) => {
                // Make sure the VM actually finished starting before
                // reporting it as ready to use.
                match client.wait_until_running(Duration::from_secs(5)).await {
                    Ok(()) => {
                        instance_ref.lock()?.state.transition(VmState::Started)?;
                        self.wake_times
                            .lock()?
                            .insert(vm_id.clone(), Instant::now());
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
        let (instance_ref, client) = self.entry_parts(vm_id)?;

        instance_ref.lock()?.state.transition(VmState::Pausing)?;

        // Pause the VM via the Firecracker API, rolling back to Started on
        // failure.
        match client.patch("/vm", &json!({"state": "Paused"})).await {
            Ok(()) => {
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
        let (instance_ref, client) = self.entry_parts(vm_id)?;

        instance_ref.lock()?.state.transition(VmState::Resuming)?;

        // Resume the VM via the Firecracker API, rolling back to Paused on
        // failure.
        match client.patch("/vm", &json!({"state": "Resumed"})).await {
            Ok(()) => {
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
        let (instance_ref, client) = self.entry_parts(vm_id)?;

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
            let child = self.spawn_fc_process(&instance_ref).await?;

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
                self.wake_times
                    .lock()?
                    .insert(vm_id.clone(), Instant::now());
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

        // Tear down the block volume (ublk device + worker + chunk files).
        // Best-effort, same rationale as the network release.
        self.storage_mgr.detach(vm_id).await;

        // Clean up runtime artifacts.
        entry.instance.lock()?.cleanup_runtime_artifacts();

        // Drop the auto-suspend bookkeeping for this VM.
        self.wake_times.lock()?.remove(vm_id);
        self.wake_locks.lock()?.remove(vm_id);
        self.activity.clear(vm_id);
        self.schedule_run_locks.remove(vm_id)?;

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
