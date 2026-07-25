use std::collections::HashMap;
use std::env;
use std::fs;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{self, Value as JsonValue};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    process,
};
use tracing::{debug, info};

use crate::common::vm::{TapName, VmConfig, VmId, VmInstance, VmState};
use crate::error::{Error, Result};

use crate::vmm::Vmm;

const FIRECRACKER_BIN: &str = "FIRECRACKER_BIN";

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
    instance: VmInstance,

    /// Firecracker API client for this VM.
    api_client: FcApiClient,

    /// Firecracker child process.
    fc: Option<tokio::process::Child>,
}

impl FcVmEntry {
    fn fc_process_id(&self) -> Option<u32> {
        self.fc.as_ref().and_then(|c| c.id())
    }

    fn is_running(&self) -> bool {
        self.instance.state == VmState::Started && self.fc_process_id().is_some()
    }

    fn socket_path(&self) -> &Path {
        &self.api_client.socket_path
    }
}

pub(crate) struct FirecrackerVmm {
    fc_bin: PathBuf,
    run_dir: PathBuf,
    vms: Mutex<HashMap<VmId, FcVmEntry>>,
}

impl FirecrackerVmm {
    pub(crate) fn new(run_dir: impl AsRef<Path>) -> Result<Self> {
        let fc_bin = from_path_or_env("firecracker", FIRECRACKER_BIN);
        debug!(?fc_bin, "Firecracker binary");
        Ok(Self {
            fc_bin,
            run_dir: run_dir.as_ref().to_path_buf(),
            vms: Mutex::new(HashMap::new()),
        })
    }

    fn spawn_fc_process(
        &self,
        vm_id: &VmId,
        error_log: &Path,
        socket_path: &Path,
    ) -> Result<process::Child> {
        let _ = fs::remove_file(socket_path); // clean stale socket
        let stderr_file = fs::File::create(error_log)?;

        debug!(vm_id = %vm_id, "spawning Firecracker");

        let child = process::Command::new(&self.fc_bin)
            .arg("--api-sock")
            .arg(&socket_path)
            .arg("--no-seccomp")
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
}

#[async_trait]
impl Vmm for FirecrackerVmm {
    async fn create_vm(&self, config: &VmConfig) -> Result<VmId> {
        let vm_id = VmId::new_random(config.project_id);
        let error_log = self.run_dir.join(format!("{vm_id}.stderr.log"));
        let socket_path = self.run_dir.join(format!("{vm_id}.sock"));

        self.vms.lock()?.insert(
            vm_id.clone(),
            FcVmEntry {
                instance: VmInstance {
                    vm_id: vm_id.clone(),
                    state: VmState::Creating,
                    tap_name: TapName::from(&vm_id),
                    guest_ip: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                    error_log: error_log.clone(),
                    vm_config: config.clone(),
                },
                api_client: FcApiClient::new(socket_path.clone()),
                fc: None,
            },
        );

        // Spawn Firecracker process.
        let child = match self.spawn_fc_process(&vm_id, &error_log, &socket_path) {
            Ok(result) => result,
            Err(e) => {
                self.vms.lock()?.remove(&vm_id);
                return Err(e);
            }
        };

        let mut vms = self.vms.lock()?;
        let vm_entry = vms
            .get_mut(&vm_id)
            .ok_or_else(|| Error::VmNotFound(vm_id.to_string()))?;
        vm_entry.fc = Some(child);
        vm_entry.instance.state = VmState::Created;

        Ok(vm_id)
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
        let _ = fs::remove_file(self.run_dir.join(format!("{vm_id}.sock")));
        let _ = fs::remove_file(self.run_dir.join(format!("{vm_id}.stderr.log")));

        info!(vm_id = %vm_id, "VM destroyed");
        Ok(())
    }
}

fn from_path_or_env(binary: &str, env_var: &str) -> PathBuf {
    if let Some(path) = env::var_os("PATH") {
        for dir in env::split_paths(&path) {
            let candidate = dir.join(binary);
            if candidate.is_file() {
                return candidate;
            }
        }
    }

    env::var(env_var)
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(binary))
}
