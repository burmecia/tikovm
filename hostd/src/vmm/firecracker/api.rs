//! Minimal client for Firecracker's HTTP-over-Unix-socket API.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{self, Value as JsonValue};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tracing::debug;

use crate::error::{Error, Result};

/// Cap on the response headers we are willing to read (they are a handful
/// of lines; anything bigger means a confused peer).
const MAX_HEADER_BYTES: usize = 8192;

/// Poll granularity of `wait_until_running`.
const RUNNING_POLL: Duration = Duration::from_millis(50);

/// HTTP client for one VM's Firecracker API socket. Each request opens a
/// fresh connection (`Connection: close`), which Firecracker handles fine.
pub(super) struct FcApiClient {
    socket_path: PathBuf,
}

impl FcApiClient {
    pub(super) fn new(socket_path: impl AsRef<Path>) -> Self {
        Self {
            socket_path: socket_path.as_ref().to_path_buf(),
        }
    }

    /// PUT a JSON body, discarding the response body on success.
    pub(super) async fn put(&self, path: &str, body: &JsonValue) -> Result<()> {
        self.request("PUT", path, Some(body)).await.map(|_| ())
    }

    /// PATCH a JSON body, discarding the response body on success.
    pub(super) async fn patch(&self, path: &str, body: &JsonValue) -> Result<()> {
        self.request("PATCH", path, Some(body)).await.map(|_| ())
    }

    /// GET and parse the response body as JSON.
    pub(super) async fn get(&self, path: &str) -> Result<JsonValue> {
        let body_str = self.request("GET", path, None).await?;
        Ok(serde_json::from_str(&body_str)?)
    }

    /// Poll the instance info endpoint until Firecracker reports the VM as
    /// `Running`, i.e. the `InstanceStart` action has fully taken effect.
    pub(super) async fn wait_until_running(&self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            let info = self.get("/").await?;
            let state = info.get("state").and_then(JsonValue::as_str).unwrap_or("");
            if state == "Running" {
                return Ok(());
            }
            if Instant::now() > deadline {
                return Err(Error::vmm(format!(
                    "VM did not reach Running state within {}s (last state: {state:?})",
                    timeout.as_secs()
                )));
            }
            tokio::time::sleep(RUNNING_POLL).await;
        }
    }

    /// One HTTP/1.1 round-trip: write the request, read headers and body,
    /// and map non-2xx responses (Firecracker's `fault_message` when
    /// present) to an error.
    async fn request(&self, method: &str, path: &str, body: Option<&JsonValue>) -> Result<String> {
        let body_str = body
            .map(std::string::ToString::to_string)
            .unwrap_or_default();
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
            if header_buf.len() > MAX_HEADER_BYTES {
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
        let body_str = String::from_utf8_lossy(&body_buf).into_owned();

        if (200..300).contains(&status_code) {
            Ok(body_str)
        } else {
            debug!("FC API {method} {path} failed: HTTP {status_code}: {body_str}",);
            let msg = serde_json::from_str::<JsonValue>(&body_str)
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
