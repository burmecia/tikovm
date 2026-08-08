//! Vsock control connection to guestd.
//!
//! Firecracker exposes each VM's virtio-vsock as a Unix socket on the host
//! (the `uds_path` of the `/vsock` device config). Connecting to it and
//! sending `CONNECT <port>\n` opens a byte stream to that vsock port in the
//! guest, where guestd listens. From then on both sides exchange
//! newline-delimited JSON (see guestd/src/main.rs for the protocol).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use crate::error::{Error, Result};

/// Vsock port guestd listens on inside the guest.
pub(super) const GUESTD_PORT: u32 = 5000;

/// Cap on the `CONNECT` handshake reply (it is a handful of bytes;
/// anything longer means a confused peer).
const MAX_REPLY_BYTES: usize = 1024;

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum GuestRequest {
    Start {
        workload_id: String,
        cmd: Vec<String>,
        env: HashMap<String, String>,
        cwd: Option<String>,
    },
    Stop {
        workload_id: String,
    },
    List,
    /// Configure guestd's auto-suspend idle detector (empty cmd disables).
    ConfigureAutoSuspend {
        idle_check_cmd: Vec<String>,
        check_interval_secs: u64,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum GuestEvent {
    Started {
        workload_id: String,
        pid: u32,
    },
    Output {
        workload_id: String,
        stream: String,
        data: String,
    },
    Exited {
        workload_id: String,
        exit_code: Option<i32>,
        signal: Option<i32>,
    },
    Error {
        workload_id: Option<String>,
        message: String,
    },
    ListResult {
        workloads: Vec<GuestWorkloadInfo>,
    },
    /// guestd's auto-suspend detector reports the guest as idle.
    Idle,
}

#[derive(Debug, Deserialize)]
pub(super) struct GuestWorkloadInfo {
    pub workload_id: String,
    pub state: String,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
}

/// Sending half of a live guest connection. Cloning shares the same
/// connection; `Arc` identity lets a dying connection task tell whether the
/// handle stored on the VM entry is still its own before clearing it.
#[derive(Clone)]
pub(super) struct GuestConnHandle {
    tx: Arc<mpsc::Sender<GuestRequest>>,
}

impl GuestConnHandle {
    pub(super) fn new(tx: mpsc::Sender<GuestRequest>) -> Self {
        Self { tx: Arc::new(tx) }
    }

    pub(super) fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }

    pub(super) fn ptr_eq(&self, other: &GuestConnHandle) -> bool {
        Arc::ptr_eq(&self.tx, &other.tx)
    }

    pub(super) async fn send(&self, request: GuestRequest) -> Result<()> {
        self.tx
            .send(request)
            .await
            .map_err(|_| Error::vmm("guest connection is closed"))
    }
}

/// Connect to a VM's vsock UDS and complete the Firecracker `CONNECT`
/// handshake, returning a stream wired to guestd's vsock port.
pub(super) async fn connect(uds_path: &Path) -> Result<UnixStream> {
    let mut stream = UnixStream::connect(uds_path).await?;
    stream
        .write_all(format!("CONNECT {GUESTD_PORT}\n").as_bytes())
        .await?;

    // The reply is a single line: "OK <port>\n" or "ERROR <message>\n".
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = stream.read(&mut byte).await?;
        if n == 0 {
            return Err(Error::vmm(
                "vsock handshake: connection closed by Firecracker",
            ));
        }
        if byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
        if line.len() > MAX_REPLY_BYTES {
            return Err(Error::vmm("vsock handshake: reply too long"));
        }
    }

    let reply = String::from_utf8_lossy(&line);
    if reply.starts_with("OK") {
        Ok(stream)
    } else {
        Err(Error::vmm(format!(
            "vsock handshake with {} failed: {reply}",
            uds_path.display()
        )))
    }
}
