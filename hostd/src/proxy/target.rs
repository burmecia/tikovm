//! Shared target resolution for both proxy modes (HTTP and raw TCP):
//! re-validate the token's claimed VM + port against live VM state and
//! return the guest IP to forward to.
//!
//! Re-validation on every connection is deliberate: a JWT only proves the
//! port was exposed at mint time. Checking the registry here means removing
//! an exposed port (or pausing/destroying the VM) revokes access
//! immediately, even before the token expires.

use std::net::IpAddr;

use hyper::StatusCode;

use super::server::ProxyState;
use super::token::Claims;
use crate::vmm::vm::{VmId, VmState};

/// A proxy failure: an HTTP status (the HTTP mode renders it as the uniform
/// JSON error body) plus a human-readable message (the TCP mode sends it as
/// a Postgres `ErrorResponse`).
pub(crate) struct ProxyError {
    pub(crate) status: StatusCode,
    pub(crate) message: String,
}

impl ProxyError {
    pub(crate) fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    /// A 500 wrapper around an internal error's message.
    fn internal(e: impl std::fmt::Display) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    }
}

/// Re-validate the claimed target against live VM state and return the
/// guest IP to forward to. A suspended VM is woken (restored from its
/// snapshot) first: the requester just sees a slow first request.
pub(crate) async fn resolve_target(
    state: &ProxyState,
    claims: &Claims,
) -> Result<IpAddr, ProxyError> {
    let vm_id = VmId::from(claims.vm_id.as_str());
    let instance_ref = state
        .vmm
        .get_vm(&vm_id)
        .await
        .map_err(ProxyError::internal)?
        .ok_or_else(|| ProxyError::new(StatusCode::NOT_FOUND, "vm not found"))?;

    let vm_state = instance_ref.lock().map_err(ProxyError::internal)?.state;

    // Wake an auto-suspended VM before validating; ensure_started
    // deduplicates concurrent wake-ups.
    if matches!(
        vm_state,
        VmState::Suspended | VmState::Suspending | VmState::Restoring
    ) {
        state
            .vmm
            .ensure_started(&vm_id)
            .await
            .map_err(|e| ProxyError::new(StatusCode::CONFLICT, e.to_string()))?;
    }

    // Validate against live state in a short scope so the instance lock
    // (a std MutexGuard, which is !Send) never crosses an await.
    let guest_ip = {
        let instance = instance_ref.lock().map_err(ProxyError::internal)?;

        if instance.state != VmState::Started {
            return Err(ProxyError::new(
                StatusCode::CONFLICT,
                format!("vm is not running (state: {:?})", instance.state),
            ));
        }

        let exposed = &instance.vm_config.network_config.exposed_ports;
        if !exposed.iter().any(|p| p.port == claims.port) {
            return Err(ProxyError::new(
                StatusCode::FORBIDDEN,
                format!("port {} is no longer exposed on this vm", claims.port),
            ));
        }

        instance
            .net
            .as_ref()
            .map(|net| net.guest_ip)
            .ok_or_else(|| ProxyError::new(StatusCode::SERVICE_UNAVAILABLE, "vm has no guest ip"))?
    };

    if vm_state != VmState::Started {
        // The VM was just woken: its listen sockets survive the snapshot,
        // but give Firecracker's resume a short grace by probing the port
        // before forwarding real traffic to it.
        wait_for_port(guest_ip, claims.port).await?;
    }

    Ok(guest_ip)
}

/// Probe `guest_ip:port` until it accepts TCP connections or the deadline
/// passes. Used after a wake-from-suspend so the first proxied request does
/// not race the guest's resumed network stack.
async fn wait_for_port(guest_ip: IpAddr, port: u16) -> Result<(), ProxyError> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match tokio::net::TcpStream::connect((guest_ip, port)).await {
            Ok(_) => return Ok(()),
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            Err(e) => {
                return Err(ProxyError::new(
                    StatusCode::BAD_GATEWAY,
                    format!("guest port {port} did not come up after wake: {e}"),
                ));
            }
        }
    }
}
