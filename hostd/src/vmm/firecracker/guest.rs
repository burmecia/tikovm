//! Guest connection management: a lazily-established, per-VM vsock control
//! connection to guestd, over which workload requests and events flow.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::mpsc,
};
use tracing::{debug, warn};

use crate::error::{Error, Result};
use crate::vmm::vm::{VmId, VmInstanceRef};
use crate::vmm::workload::{WorkloadId, WorkloadLogEntry};

use super::vmm::{FcVmEntry, FirecrackerVmm};
use super::vsock::{self, GuestConnHandle, GuestEvent, GuestRequest};

impl FirecrackerVmm {
    /// Get a live vsock connection to the VM's guestd, establishing one if
    /// needed. Retries for up to a minute: the host-side UDS listener exists
    /// as soon as Firecracker is configured, but guestd only accepts once the
    /// guest has booted far enough to start it.
    pub(super) async fn guest_conn(&self, vm_id: &VmId) -> Result<GuestConnHandle> {
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
                    wl.mark_running(Some(pid));
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
                let log_path = WorkloadId(workload_id).log_path(workloads_dir);
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
                                "running" => wl.mark_running(None),
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
