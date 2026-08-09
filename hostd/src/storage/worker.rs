//! Parent-side management of `ublk-worker` subprocesses: spawn, parse the
//! device id the worker prints on stdout, and kill on demand. Crash
//! monitoring/respawn lives in `manager.rs`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tracing::debug;

use crate::error::{Error, Result};

pub(crate) struct WorkerHandle {
    pub dev_id: i32,
    pub dev_path: PathBuf,
    pub child: Child,
}

/// Spawn `hostd ublk-worker` for `volume_dir`. When `recover` is set the
/// worker reattaches to the existing device `dev_id` (left behind by a
/// dead worker; `size_mb`/`chunk_kb` are ignored — the volume's meta.json
/// is authoritative).
pub(crate) async fn spawn(
    volume_dir: &Path,
    size_mb: Option<u64>,
    chunk_kb: Option<u32>,
    recover: bool,
    dev_id: i32,
    queues: u16,
    depth: u16,
) -> Result<WorkerHandle> {
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.arg("ublk-worker")
        .arg("--dir")
        .arg(volume_dir)
        .arg("-n")
        .arg(dev_id.to_string())
        .arg("--queues")
        .arg(queues.to_string())
        .arg("--depth")
        .arg(depth.to_string());
    if recover {
        cmd.arg("--recover");
    } else {
        cmd.arg("--size-mb").arg(
            size_mb
                .ok_or_else(|| Error::storage("size_mb required for new volume"))?
                .to_string(),
        );
        if let Some(kb) = chunk_kb {
            cmd.arg("--chunk-kb").arg(kb.to_string());
        }
    }
    // stdout carries the "device id: N" handshake line; stderr is inherited
    // so worker logs (tracing, RUST_LOG applies) land in hostd's log.
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(false)
        .spawn()
        .map_err(|e| Error::storage(format!("spawn ublk-worker: {e}")))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::storage("ublk-worker stdout not piped"))?;
    // Skip any noise lines until the handshake arrives (or the timeout
    // fires); the worker logs to stderr, so stdout should carry exactly
    // the one handshake line, but be liberal in what we accept.
    let handshake = async {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            let n = reader.read_line(&mut line).await?;
            if n == 0 {
                return Err(Error::storage("ublk-worker exited before handshake"));
            }
            if let Some(id) = line.trim().strip_prefix("device id: ") {
                return id.parse::<i32>().map_err(|e| {
                    Error::storage(format!("bad ublk-worker handshake {line:?}: {e}"))
                });
            }
        }
    };
    let id = match tokio::time::timeout(Duration::from_secs(15), handshake).await {
        Ok(Ok(id)) => id,
        Ok(Err(e)) => return Err(e),
        Err(_) => return Err(Error::storage("ublk-worker handshake timed out")),
    };

    // The kernel (devtmpfs) creates the node with the device; allow a
    // short settle window.
    let dev_path = PathBuf::from(format!("/dev/ublkb{id}"));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !dev_path.exists() {
        if Instant::now() > deadline {
            return Err(Error::storage(format!(
                "{} did not appear after ublk-worker handshake",
                dev_path.display()
            )));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    debug!(dev_id = id, path = %dev_path.display(), "ublk worker up");
    Ok(WorkerHandle {
        dev_id: id,
        dev_path,
        child,
    })
}

/// Delete the ublk device and make sure the worker process is gone.
/// Best-effort: callers log, workers that already exited are fine.
pub(crate) async fn stop(dev_id: i32, child: &mut Child) -> Result<()> {
    // Asking the kernel to delete the device lets the worker's queues come
    // down cleanly (run_target returns); kill is the fallback.
    let del = tokio::task::spawn_blocking(move || {
        libublk::ctrl::UblkCtrl::new_simple(dev_id)
            .and_then(|c| c.del_dev())
            .map_err(|e| Error::storage(format!("del ublk device {dev_id}: {e}")))
    })
    .await
    .map_err(|e| Error::storage(format!("join del_dev: {e}")))?;
    if let Err(e) = del {
        debug!(error = %e, "del_dev failed; killing worker");
    }
    if child.try_wait()?.is_none() {
        child.kill().await?;
    }
    Ok(())
}
