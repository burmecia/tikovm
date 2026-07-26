use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{self, Value as JsonValue, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    process,
};
use tracing::{debug, info, warn};

use crate::common::vm::{VmConfig, VmId, VmInstance, VmInstanceRef, VmSnapshot, VmState};
use crate::error::{Error, Result};

use crate::vmm::Vmm;

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
}

impl FcVmEntry {
    fn new(instance: VmInstanceRef) -> Self {
        let api_client = Arc::new(FcApiClient::new(&instance.lock().unwrap().socket_path));
        Self {
            instance,
            api_client,
            fc: None,
        }
    }
}

pub(crate) struct FirecrackerVmm {
    fc_bin: PathBuf,
    assets_dir: PathBuf,
    work_dir: PathBuf,
    vms: Mutex<HashMap<VmId, FcVmEntry>>,
}

impl FirecrackerVmm {
    pub(crate) fn new(assets_dir: impl AsRef<Path>, work_dir: impl AsRef<Path>) -> Result<Self> {
        let fc_bin = from_path_or_env("firecracker", ENV_FIRECRACKER_BIN);
        debug!(?fc_bin, "Firecracker binary");
        Ok(Self {
            fc_bin,
            assets_dir: assets_dir.as_ref().to_path_buf(),
            work_dir: work_dir.as_ref().to_path_buf(),
            vms: Mutex::new(HashMap::new()),
        })
    }

    fn spawn_fc_process(&self, instance_ref: VmInstanceRef) -> Result<process::Child> {
        let (vm_id, socket_path, error_log) = {
            let instance = instance_ref.lock()?;
            (
                instance.vm_id.clone(),
                instance.socket_path.clone(),
                instance.error_log.clone(),
            )
        };

        let _ = fs::remove_file(&socket_path); // clean stale socket
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

        // Configure boot source (kernel + initramfs).
        let boot_source = json!({
            "kernel_image_path": instance.kernel_path.to_string_lossy(),
            "boot_args": instance.boot_args,
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

        Ok(())
    }
}

#[async_trait]
impl Vmm for FirecrackerVmm {
    async fn create_vm(&self, config: &VmConfig) -> Result<VmId> {
        let instance = VmInstance::new(config, &self.assets_dir, &self.work_dir)?;
        let vm_id = instance.vm_id.clone();
        let instance_ref = instance.into_ref();

        self.vms
            .lock()?
            .insert(vm_id.clone(), FcVmEntry::new(instance_ref.clone()));

        // Spawn Firecracker before registering the VM, so a spawn failure
        // leaves nothing behind in the map.
        let child = self.spawn_fc_process(instance_ref.clone())?;

        // Configure the VM.
        self.configure_vm(instance_ref.clone()).await?;

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

            let snapshot = VmSnapshot::new(vm_id, &self.work_dir);
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
