//! `StorageManager`: ties VM lifecycle to per-VM ublk block volumes.
//!
//! One volume per VM (`<storage_root>/proj-<project_id>/<vm_id>/`), one
//! `ublk-worker` subprocess per volume, created at `create_vm` and torn
//! down at `destroy_vm`. No persisted state: like the Firecracker
//! children, nothing storage-related survives a hostd restart (VMs don't
//! either), so there is nothing to reconcile on startup.
//!
//! Worker crash handling: the ublk device is created with
//! `UBLK_F_USER_RECOVERY`, so a dead worker leaves the device parked
//! instead of failed; the monitor respawns the worker with `--recover`
//! (bounded, backed off). If respawns are exhausted the device stays dead
//! and the guest sees EIO — the same contract as a dead disk.
//!
//! Ownership model: the monitor task owns the worker `Child` (it is the
//! reaper); the shared map entry carries the child's pid and the ublk
//! device id, which is all `detach` needs to stop the pair (SIGKILL by
//! pid, then `del_dev`) without racing the monitor's `wait()`. Removing
//! the map entry is the detach signal: a monitor that sees its entry gone
//! exits without respawning.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tracing::{debug, error, info, warn};

use super::worker::{self, WorkerHandle};
use crate::error::{Error, Result};
use crate::vmm::vm::{BlockStorageConfig, VmId};

const WORKER_QUEUES: u16 = 8;
const WORKER_DEPTH: u16 = 64;
const MAX_RESPAWNS: u32 = 3;

struct WorkerEntry {
    dev_id: i32,
    pid: u32,
    volume_dir: PathBuf,
    respawns: u32,
}

pub(crate) struct StorageManager {
    storage_root: PathBuf,
    workers: Arc<Mutex<HashMap<VmId, WorkerEntry>>>,
}

impl StorageManager {
    pub(crate) fn new(storage_root: impl AsRef<Path>) -> Self {
        Self {
            storage_root: storage_root.as_ref().to_path_buf(),
            workers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Create (or reuse) the VM's volume, spawn its worker, format a fresh
    /// volume ext4, and return the `/dev/ublkbN` path to attach.
    pub(crate) async fn attach(
        self: &Arc<Self>,
        vm_id: &VmId,
        project_id: u64,
        cfg: &BlockStorageConfig,
    ) -> Result<PathBuf> {
        if !self.storage_root.is_dir() {
            return Err(Error::storage(format!(
                "storage root {} is not a directory (S3 Files not mounted?)",
                self.storage_root.display()
            )));
        }
        let volume_dir = self
            .storage_root
            .join(format!("proj-{project_id}"))
            .join(&**vm_id);
        let is_new = !volume_dir.join("meta.json").exists();
        std::fs::create_dir_all(&volume_dir)?;

        let handle = worker::spawn(
            &volume_dir,
            Some(u64::from(cfg.size_mb)),
            cfg.chunk_kb,
            false,
            -1,
            WORKER_QUEUES,
            WORKER_DEPTH,
        )
        .await?;

        if is_new && let Err(e) = mkfs_ext4(&handle.dev_path).await {
            let mut child = handle.child;
            let _ = worker::stop(handle.dev_id, &mut child).await;
            let _ = std::fs::remove_dir_all(&volume_dir);
            return Err(e);
        }

        let dev_path = handle.dev_path.clone();
        let pid = handle
            .child
            .id()
            .ok_or_else(|| Error::storage("ublk worker exited during attach"))?;
        self.workers.lock()?.insert(
            vm_id.clone(),
            WorkerEntry {
                dev_id: handle.dev_id,
                pid,
                volume_dir,
                respawns: 0,
            },
        );
        self.spawn_monitor(vm_id.clone(), handle.child);
        info!(vm_id = %vm_id, dev = %dev_path.display(), new = is_new, "block storage attached");
        Ok(dev_path)
    }

    /// Tear down the VM's volume: remove the map entry (the monitor's
    /// no-respawn signal), kill the worker by pid, delete the ublk device,
    /// and delete the volume directory in the background — deleting many
    /// small files from S3 Files is slow, and a failure here must not
    /// block VM destruction. Best-effort throughout, like
    /// `NetworkManager::release`.
    pub(crate) async fn detach(&self, vm_id: &VmId) {
        let entry = self.workers.lock().ok().and_then(|mut w| w.remove(vm_id));
        let Some(entry) = entry else { return };

        // The monitor owns and reaps the Child; signal by pid.
        // SAFETY: plain signal send to a child we spawned; errors ignored
        // (ESRCH just means the worker already exited).
        unsafe { libc::kill(entry.pid as i32, libc::SIGKILL) };
        let dev_id = entry.dev_id;
        let dir = entry.volume_dir.clone();
        let del = tokio::task::spawn_blocking(move || {
            libublk::ctrl::UblkCtrl::new_simple(dev_id)
                .and_then(|c| c.del_dev())
                .map_err(|e| Error::storage(format!("del ublk device {dev_id}: {e}")))
        })
        .await;
        match del {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => warn!(vm_id = %vm_id, error = %e, "del ublk device failed"),
            Err(e) => warn!(vm_id = %vm_id, error = %e, "join del_dev failed"),
        }

        tokio::task::spawn_blocking(move || {
            if let Err(e) = std::fs::remove_dir_all(&dir) {
                warn!(dir = %dir.display(), error = %e, "failed to remove volume directory");
            }
        });
        debug!(vm_id = %vm_id, "block storage detached");
    }

    /// Delete any ublk devices left behind by a previous hostd (its
    /// workers die with it via PR_SET_PDEATHSIG, but a
    /// `UBLK_F_USER_RECOVERY` device stays parked in the kernel until
    /// explicitly deleted). At hostd startup no workers of ours exist yet,
    /// so every ublk device present is by definition a leftover — the same
    /// assumption class as `NetworkManager` deleting stray `tbr-*`
    /// bridges. (Name-based filtering would need libublk's per-device
    /// JSON state, which does not survive.) Volume directories on the
    /// storage root are NOT deleted here (they are data); their VMs are
    /// gone after a restart, so warn instead.
    pub(crate) fn reconcile_on_startup(&self) {
        libublk::ctrl::UblkCtrl::for_each_dev_id(|dev_id| {
            warn!(dev_id, "reconcile: deleting leftover ublk device");
            match libublk::ctrl::UblkCtrl::new_simple(dev_id as i32) {
                Ok(ctrl) => {
                    if let Err(e) = ctrl.del_dev() {
                        warn!(dev_id, error = %e, "reconcile: failed to delete ublk device");
                    }
                }
                Err(e) => warn!(dev_id, error = %e, "reconcile: failed to open ublk device"),
            }
        });

        if let Ok(entries) = std::fs::read_dir(&self.storage_root) {
            let stale: Vec<String> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with("proj-"))
                .collect();
            if !stale.is_empty() {
                warn!(
                    storage_root = %self.storage_root.display(),
                    ?stale,
                    "reconcile: stale volume directories from before the restart; not deleting data automatically"
                );
            }
        }
    }

    /// Watch the worker (owning its `Child`); on unexpected exit, respawn
    /// with `--recover` (bounded, backed off). See the module docs for the
    /// ownership model.
    fn spawn_monitor(self: &Arc<Self>, vm_id: VmId, mut child: tokio::process::Child) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                let status = child.wait().await;

                let (dev_id, volume_dir, respawns) = {
                    let workers = match this.workers.lock() {
                        Ok(w) => w,
                        Err(_) => return,
                    };
                    let Some(entry) = workers.get(&vm_id) else {
                        return; // detached while we waited: expected exit
                    };
                    (entry.dev_id, entry.volume_dir.clone(), entry.respawns)
                };
                if respawns >= MAX_RESPAWNS {
                    error!(
                        vm_id = %vm_id,
                        status = ?status.ok(),
                        "ublk worker respawn limit reached; block device stays dead (guest sees EIO)"
                    );
                    return;
                }
                warn!(
                    vm_id = %vm_id,
                    status = ?status.ok(),
                    "ublk worker exited unexpectedly; respawning with --recover"
                );

                tokio::time::sleep(Duration::from_secs(1 << respawns)).await;
                match worker::spawn(
                    &volume_dir,
                    None,
                    None,
                    true,
                    dev_id,
                    WORKER_QUEUES,
                    WORKER_DEPTH,
                )
                .await
                {
                    Ok(WorkerHandle {
                        child: new_child, ..
                    }) => {
                        let pid = new_child.id();
                        let inserted = {
                            let mut workers = match this.workers.lock() {
                                Ok(w) => w,
                                Err(_) => return,
                            };
                            match workers.get_mut(&vm_id) {
                                Some(entry) => {
                                    if let Some(pid) = pid {
                                        entry.pid = pid;
                                    }
                                    entry.respawns += 1;
                                    info!(vm_id = %vm_id, attempt = entry.respawns, "ublk worker recovered");
                                    true
                                }
                                None => false,
                            }
                        };
                        if !inserted {
                            // VM destroyed mid-respawn: don't leak it.
                            let mut c = new_child;
                            let _ = worker::stop(dev_id, &mut c).await;
                            return;
                        }
                        child = new_child;
                    }
                    Err(e) => {
                        error!(vm_id = %vm_id, error = %e, "ublk worker respawn failed");
                        let mut workers = match this.workers.lock() {
                            Ok(w) => w,
                            Err(_) => return,
                        };
                        match workers.get_mut(&vm_id) {
                            Some(entry) => {
                                entry.respawns += 1;
                                if entry.respawns >= MAX_RESPAWNS {
                                    error!(vm_id = %vm_id, "ublk worker respawn limit reached");
                                    return;
                                }
                                // Retry from the top of the loop. `child`
                                // is already exited; waiting on it again
                                // returns immediately, so the loop doubles
                                // as the retry path.
                            }
                            None => return,
                        }
                    }
                }
            }
        });
    }
}

/// Format a fresh volume ext4 with a stable label the guest mount unit
/// references (`/dev/disk/by-label/tikovm-data`).
async fn mkfs_ext4(dev_path: &Path) -> Result<()> {
    let status = tokio::time::timeout(
        Duration::from_secs(180),
        tokio::process::Command::new("mkfs.ext4")
            .args(["-F", "-q", "-m", "0", "-L", "tikovm-data"])
            .arg(dev_path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status(),
    )
    .await
    .map_err(|_| Error::storage(format!("mkfs.ext4 {} timed out", dev_path.display())))?
    .map_err(|e| Error::storage(format!("spawn mkfs.ext4: {e}")))?;
    if !status.success() {
        return Err(Error::storage(format!(
            "mkfs.ext4 {} failed: {status}",
            dev_path.display()
        )));
    }
    Ok(())
}
