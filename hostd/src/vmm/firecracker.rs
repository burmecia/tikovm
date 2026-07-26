use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{self, Value as JsonValue, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    process,
    sync::mpsc,
};
use tracing::{debug, info, warn};

use crate::common::vm::{TapName, VmConfig, VmId, VmInstance, VmInstanceRef, VmSnapshot, VmState};
use crate::common::workload::{Workload, WorkloadId, WorkloadLogEntry, WorkloadSpec};
use crate::error::{Error, Result};
use crate::net::NetworkManager;

use crate::vmm::Vmm;
use crate::vmm::vsock::{self, GuestConnHandle, GuestEvent, GuestRequest};

const ENV_FIRECRACKER_BIN: &str = "FIRECRACKER_BIN";

struct FcApiClient {
    socket_path: PathBuf,
}

impl FcApiClient {
    fn new(socket_path: impl AsRef<Path>) -> Self {
        Self {
            socket_path: socket_path.as_ref().to_path_buf(),
        }
    }

    async fn put(&self, path: &str, body: &JsonValue) -> Result<()> {
        let _ = self.request("PUT", path, Some(body)).await?;
        Ok(())
    }

    async fn patch(&self, path: &str, body: &JsonValue) -> Result<()> {
        let _ = self.request("PATCH", path, Some(body)).await?;
        Ok(())
    }

    async fn get(&self, path: &str) -> Result<JsonValue> {
        let body_str = self.request("GET", path, None).await?;
        Ok(serde_json::from_str(&body_str)?)
    }

    /// Poll the instance info endpoint until Firecracker reports the VM as
    /// `Running`, i.e. the InstanceStart action has fully taken effect.
    async fn wait_until_running(&self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            let state = self
                .get("/")
                .await?
                .get("state")
                .and_then(JsonValue::as_str)
                .unwrap_or("")
                .to_string();
            if state == "Running" {
                return Ok(());
            }
            if Instant::now() > deadline {
                return Err(Error::vmm(format!(
                    "VM did not reach Running state within {}s (last state: {state:?})",
                    timeout.as_secs()
                )));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn request(&self, method: &str, path: &str, body: Option<&JsonValue>) -> Result<String> {
        let body_str = body.map(|b| b.to_string()).unwrap_or_default();
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
             Content-Length: {len}\r\nConnection: close\r\n\r\n{body}",
            len = body_str.len(),
            body = body_str,
        );

        let mut stream = UnixStream::connect(&self.socket_path).await?;
        stream.write_all(request.as_bytes()).await?;

        let mut header_buf = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            let n = stream.read(&mut byte).await?;
            if n == 0 {
                break;
            }
            header_buf.push(byte[0]);
            if header_buf.ends_with(b"\r\n\r\n") {
                break;
            }
            if header_buf.len() > 8192 {
                return Err(Error::io_other("FC API response headers too large"));
            }
        }

        let header_str = String::from_utf8_lossy(&header_buf);
        let status_line = header_str.lines().next().unwrap_or("");
        let status_code = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(0);

        let content_length: usize = header_str
            .lines()
            .find_map(|line| {
                let line = line.to_lowercase();
                line.strip_prefix("content-length:")
                    .and_then(|rest| rest.trim().parse().ok())
            })
            .unwrap_or(0);

        let mut body_buf = vec![0u8; content_length];
        if content_length > 0 {
            stream.read_exact(&mut body_buf).await?;
        }
        let body_str = String::from_utf8_lossy(&body_buf).to_string();

        if (200..300).contains(&status_code) {
            Ok(body_str)
        } else {
            debug!("FC API {method} {path} failed: HTTP {status_code}: {body_str}",);
            let msg = serde_json::from_str::<serde_json::Value>(&body_str)
                .ok()
                .and_then(|v| {
                    v.get("fault_message")
                        .and_then(|f| f.as_str())
                        .map(String::from)
                })
                .unwrap_or_else(|| format!("HTTP {status_code}: {body_str}"));
            Err(Error::io_other(format!("FC API {method} {path}: {msg}")))
        }
    }
}

struct FcVmEntry {
    instance: VmInstanceRef,

    /// Firecracker API client for this VM.
    api_client: Arc<FcApiClient>,

    /// Firecracker child process.
    fc: Option<tokio::process::Child>,

    /// Workloads running (or finished) in this VM via guestd.
    workloads: HashMap<WorkloadId, Workload>,

    /// Live vsock control connection to guestd, if any.
    guest_conn: Option<GuestConnHandle>,

    /// Directory holding per-workload log files (`<workload_id>.log`, JSON lines).
    workloads_dir: PathBuf,
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
    fc_bin: PathBuf,
    assets_dir: PathBuf,
    work_dir: PathBuf,
    net_mgr: Arc<NetworkManager>,
    vms: Arc<Mutex<HashMap<VmId, FcVmEntry>>>,
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

    fn spawn_fc_process(&self, instance_ref: VmInstanceRef) -> Result<process::Child> {
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

    async fn configure_vm(&self, instance_ref: VmInstanceRef) -> Result<()> {
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

/// Guest connection management: a lazily-established, per-VM vsock control
/// connection to guestd, over which workload requests and events flow.
impl FirecrackerVmm {
    /// Get a live vsock connection to the VM's guestd, establishing one if
    /// needed. Retries for up to a minute: the host-side UDS listener exists
    /// as soon as Firecracker is configured, but guestd only accepts once the
    /// guest has booted far enough to start it.
    async fn guest_conn(&self, vm_id: &VmId) -> Result<GuestConnHandle> {
        {
            let vms = self.vms.lock()?;
            if let Some(handle) = vms
                .get(vm_id)
                .and_then(|entry| entry.guest_conn.clone())
                .filter(|handle| !handle.is_closed())
            {
                return Ok(handle);
            }
        }

        let (uds_path, instance_ref) = {
            let vms = self.vms.lock()?;
            let entry = vms
                .get(vm_id)
                .ok_or_else(|| Error::VmNotFound(vm_id.to_string()))?;
            (
                entry.instance.lock()?.vsock_uds_path.clone(),
                entry.instance.clone(),
            )
        };

        let mut last_err: Option<Error> = None;
        for _ in 0..60 {
            match vsock::connect(&uds_path).await {
                Ok(stream) => return self.install_guest_conn(vm_id, instance_ref, stream).await,
                Err(e) => {
                    last_err = Some(e);
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
        Err(last_err.unwrap_or_else(|| Error::vmm("vsock connect failed")))
    }

    /// Store a freshly connected stream on the VM entry and spawn the task
    /// driving it, then ask guestd for its workload table so a reconnect
    /// resyncs state hostd may have missed while disconnected.
    async fn install_guest_conn(
        &self,
        vm_id: &VmId,
        instance_ref: VmInstanceRef,
        stream: UnixStream,
    ) -> Result<GuestConnHandle> {
        let (tx, rx) = mpsc::channel(64);
        let handle = GuestConnHandle::new(tx);
        {
            let mut vms = self.vms.lock()?;
            let entry = vms
                .get_mut(vm_id)
                .ok_or_else(|| Error::VmNotFound(vm_id.to_string()))?;
            entry.guest_conn = Some(handle.clone());
        }

        let workloads_dir = instance_ref.lock()?.work_dir.join("workloads");
        tokio::spawn(Self::run_guest_conn(
            vm_id.clone(),
            Arc::clone(&self.vms),
            workloads_dir,
            handle.clone(),
            stream,
            rx,
        ));

        handle.send(GuestRequest::List).await?;
        Ok(handle)
    }

    /// Forward requests to and read events from guestd until either side
    /// drops, then clear the stored handle so the next operation reconnects.
    async fn run_guest_conn(
        vm_id: VmId,
        vms: Arc<Mutex<HashMap<VmId, FcVmEntry>>>,
        workloads_dir: PathBuf,
        handle: GuestConnHandle,
        stream: UnixStream,
        mut rx: mpsc::Receiver<GuestRequest>,
    ) {
        let (read_half, mut write_half) = stream.into_split();
        let mut lines = BufReader::new(read_half).lines();
        loop {
            tokio::select! {
                line = lines.next_line() => {
                    match line {
                        Ok(Some(line)) => match serde_json::from_str::<GuestEvent>(&line) {
                            Ok(event) => Self::handle_guest_event(&vm_id, &vms, &workloads_dir, event),
                            Err(e) => warn!(vm_id = %vm_id, error = %e, "malformed event from guestd"),
                        },
                        Ok(None) => break, // guestd closed the connection
                        Err(e) => {
                            debug!(vm_id = %vm_id, error = %e, "guest connection read error");
                            break;
                        }
                    }
                }
                request = rx.recv() => {
                    match request {
                        Some(request) => {
                            let mut buf = match serde_json::to_string(&request) {
                                Ok(s) => s,
                                Err(e) => {
                                    warn!(vm_id = %vm_id, error = %e, "failed to serialize guest request");
                                    continue;
                                }
                            };
                            buf.push('\n');
                            if let Err(e) = write_half.write_all(buf.as_bytes()).await {
                                debug!(vm_id = %vm_id, error = %e, "guest connection write error");
                                break;
                            }
                        }
                        None => break, // all senders dropped
                    }
                }
            }
        }

        // Clear the stored handle only if it is still this connection's, so a
        // newer connection installed by a concurrent reconnect survives.
        if let Ok(mut vms) = vms.lock()
            && let Some(entry) = vms.get_mut(&vm_id)
            && entry.guest_conn.as_ref().is_some_and(|h| h.ptr_eq(&handle))
        {
            entry.guest_conn = None;
        }
        debug!(vm_id = %vm_id, "guest connection closed");
    }

    fn handle_guest_event(
        vm_id: &VmId,
        vms: &Arc<Mutex<HashMap<VmId, FcVmEntry>>>,
        workloads_dir: &Path,
        event: GuestEvent,
    ) {
        match event {
            GuestEvent::Started { workload_id, pid } => {
                debug!(vm_id = %vm_id, workload_id, pid, "workload started in guest");
                if let Ok(mut vms) = vms.lock()
                    && let Some(entry) = vms.get_mut(vm_id)
                    && let Some(wl) = entry.workloads.get_mut(&WorkloadId(workload_id))
                {
                    wl.mark_running();
                }
            }
            GuestEvent::Output {
                workload_id,
                stream,
                data,
            } => {
                let log_entry = WorkloadLogEntry {
                    ts: chrono::Utc::now(),
                    stream,
                    data,
                };
                let log_path = workloads_dir.join(format!("{workload_id}.log"));
                if let Ok(mut file) = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&log_path)
                    && let Ok(json) = serde_json::to_string(&log_entry)
                {
                    let _ = writeln!(file, "{json}");
                }
            }
            GuestEvent::Exited {
                workload_id,
                exit_code,
                signal,
            } => {
                debug!(vm_id = %vm_id, workload_id, ?exit_code, ?signal, "workload exited in guest");
                if let Ok(mut vms) = vms.lock()
                    && let Some(entry) = vms.get_mut(vm_id)
                    && let Some(wl) = entry.workloads.get_mut(&WorkloadId(workload_id))
                {
                    wl.mark_finished(exit_code, signal);
                }
            }
            GuestEvent::Error {
                workload_id,
                message,
            } => {
                warn!(vm_id = %vm_id, ?workload_id, message, "guestd error");
                if let Some(workload_id) = workload_id
                    && let Ok(mut vms) = vms.lock()
                    && let Some(entry) = vms.get_mut(vm_id)
                    && let Some(wl) = entry.workloads.get_mut(&WorkloadId(workload_id))
                {
                    wl.mark_failed();
                }
            }
            // Reconcile host state with guestd's table after a reconnect:
            // workloads hostd still thinks are active may have exited (or
            // merely started) while the connection was down.
            GuestEvent::ListResult { workloads } => {
                if let Ok(mut vms) = vms.lock()
                    && let Some(entry) = vms.get_mut(vm_id)
                {
                    for info in workloads {
                        if let Some(wl) = entry.workloads.get_mut(&WorkloadId(info.workload_id)) {
                            match info.state.as_str() {
                                "running" => wl.mark_running(),
                                "exited" => wl.mark_finished(info.exit_code, info.signal),
                                _ => {}
                            }
                        }
                    }
                }
            }
        }
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
                if let Some(mut child) = old_child {
                    if let Err(e) = child.kill().await {
                        warn!(vm_id = %vm_id, error = %e, "failed to kill Firecracker process after snapshotting");
                    }
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
        if spec.argv.is_empty() {
            return Err(Error::vmm("workload argv must not be empty"));
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
            argv: workload.spec.argv.clone(),
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
            entry.workloads_dir.join(format!("{workload_id}.log"))
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
        if let Some(mut child) = entry.fc.take() {
            if let Err(e) = child.kill().await {
                debug!(vm_id = %vm_id, error = %e, "failed to kill Firecracker process");
            }
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

fn from_path_or_env(binary: &str, env_var: &str) -> PathBuf {
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
