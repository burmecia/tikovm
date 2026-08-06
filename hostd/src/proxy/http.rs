//! HTTP request handling for the proxy: authenticate the request with its
//! bearer JWT, re-validate the target against live VM state, and forward the
//! request to `http://<guest_ip>:<port>`.
//!
//! Re-validation on every request is deliberate: a JWT only proves the port
//! was exposed at mint time. Checking the registry here means removing an
//! exposed port (or pausing/destroying the VM) revokes access immediately,
//! even before the token expires.

use bytes::Bytes;
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::header::{
    AUTHORIZATION, CONNECTION, CONTENT_TYPE, HOST, PROXY_AUTHENTICATE, PROXY_AUTHORIZATION, TE,
    TRAILER, TRANSFER_ENCODING, UPGRADE,
};
use hyper::{Request, Response, StatusCode, body::Incoming, header::HeaderName};
use serde_json::json;
use std::pin::Pin;
use std::task::{Context, Poll};
use tracing::{debug, warn};

use super::server::ProxyState;
use super::token::{Claims, Proto};
use crate::vmm::activity::ActivityGuard;
use crate::vmm::vm::{VmId, VmState};

/// Body type shared by proxied upstream responses and local error responses.
pub(crate) type ProxyBody = BoxBody<Bytes, hyper::Error>;

/// Hop-by-hop headers that must not be forwarded in either direction, plus
/// `host` (rewritten for the upstream) and `authorization` (the proxy JWT is
/// hostd's, not the guest's).
static STRIPPED_REQUEST_HEADERS: [HeaderName; 10] = [
    HOST,
    AUTHORIZATION,
    CONNECTION,
    HeaderName::from_static("keep-alive"),
    PROXY_AUTHENTICATE,
    PROXY_AUTHORIZATION,
    TE,
    TRAILER,
    TRANSFER_ENCODING,
    UPGRADE,
];

static STRIPPED_RESPONSE_HEADERS: [HeaderName; 8] = [
    CONNECTION,
    HeaderName::from_static("keep-alive"),
    PROXY_AUTHENTICATE,
    PROXY_AUTHORIZATION,
    TE,
    TRAILER,
    TRANSFER_ENCODING,
    UPGRADE,
];

/// A proxy failure rendered as the same uniform JSON error body the
/// management API uses: `{"error": {"code": <http status>, "message": ...}}`.
struct ProxyError {
    status: StatusCode,
    message: String,
}

impl ProxyError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

/// Service entry point: never fails, errors become JSON error responses.
pub(crate) async fn handle(state: ProxyState, req: Request<Incoming>) -> Response<ProxyBody> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let response = match forward(&state, req).await {
        Ok(response) => response,
        Err(err) => error_response(&err),
    };
    debug!(%method, %path, status = %response.status(), "proxied request");
    response
}

async fn forward(
    state: &ProxyState,
    req: Request<Incoming>,
) -> Result<Response<ProxyBody>, ProxyError> {
    let claims = authenticate(state, &req)?;
    // Track the request for auto-suspend: this is both the HTTP activity
    // signal and the gate's in-flight count. The guard lives in the
    // response body so a streamed reply keeps the VM "active" until the
    // last byte is sent.
    let guard = state.vmm.activity().track(&VmId::from(claims.vm_id.clone()));
    let guest_ip = resolve_target(state, &claims).await?;

    let authority = format!("{guest_ip}:{}", claims.port);
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");
    let upstream_uri = format!("http://{authority}{path_and_query}");

    let (parts, body) = req.into_parts();
    let mut builder = Request::builder().method(parts.method).uri(&upstream_uri);
    for (name, value) in parts.headers.iter() {
        if !STRIPPED_REQUEST_HEADERS.contains(name) {
            builder = builder.header(name, value);
        }
    }
    let upstream_req = builder
        .header(HOST, &authority)
        .body(body.boxed())
        .map_err(|e| ProxyError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let upstream_resp = state
        .client
        .request(upstream_req)
        .await
        .map_err(|e| {
            warn!(error = %e, "upstream request failed");
            ProxyError::new(
                StatusCode::BAD_GATEWAY,
                format!("failed to reach guest port {}: {e}", claims.port),
            )
        })?;

    let (parts, body) = upstream_resp.into_parts();
    let mut response = Response::builder().status(parts.status);
    for (name, value) in parts.headers.iter() {
        if !STRIPPED_RESPONSE_HEADERS.contains(name) {
            response = response.header(name, value);
        }
    }
    response
        .body(
            GuardedBody {
                inner: body.boxed(),
                _guard: guard,
            }
            .boxed(),
        )
        .map_err(|e| ProxyError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

/// A proxied response body that keeps the VM's activity guard alive until
/// the body is fully sent or dropped: `in_flight` covers the whole streamed
/// response, so auto-suspend cannot snapshot a VM mid-reply.
struct GuardedBody {
    inner: ProxyBody,
    _guard: ActivityGuard,
}

impl hyper::body::Body for GuardedBody {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
        Pin::new(&mut self.inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        self.inner.size_hint()
    }
}

/// Extract and verify the bearer JWT; the claims name the target VM + port.
fn authenticate(state: &ProxyState, req: &Request<Incoming>) -> Result<Claims, ProxyError> {
    let unauthorized = |msg: &str| ProxyError::new(StatusCode::UNAUTHORIZED, msg.to_string());
    let token = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or_else(|| unauthorized("missing bearer token"))?;
    let claims = state
        .tokens
        .verify(token)
        .map_err(|e| unauthorized(&e.to_string()))?;
    if claims.proto != Proto::Http {
        return Err(ProxyError::new(
            StatusCode::BAD_REQUEST,
            "token is not valid for HTTP proxying",
        ));
    }
    Ok(claims)
}

/// Re-validate the claimed target against live VM state and return the
/// guest IP to forward to. A suspended VM is woken (restored from its
/// snapshot) first: the requester just sees a slow first request.
async fn resolve_target(
    state: &ProxyState,
    claims: &Claims,
) -> Result<std::net::IpAddr, ProxyError> {
    let vm_id = VmId::from(claims.vm_id.clone());
    let instance_ref = state
        .vmm
        .get_vm(&vm_id)
        .await
        .map_err(|e| ProxyError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or_else(|| ProxyError::new(StatusCode::NOT_FOUND, "vm not found"))?;

    let vm_state = instance_ref
        .lock()
        .map_err(|e| ProxyError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .state;

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
        let instance = instance_ref
            .lock()
            .map_err(|e| ProxyError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

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
async fn wait_for_port(guest_ip: std::net::IpAddr, port: u16) -> Result<(), ProxyError> {
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

fn error_response(err: &ProxyError) -> Response<ProxyBody> {
    let body = json!({
        "error": {
            "code": err.status.as_u16(),
            "message": err.message,
        }
    });
    Response::builder()
        .status(err.status)
        .header(CONTENT_TYPE, "application/json")
        .body(full_body(body.to_string()))
        .expect("error response is always well-formed")
}

fn full_body(bytes: impl Into<Bytes>) -> ProxyBody {
    Full::new(bytes.into())
        .map_err(|never| match never {})
        .boxed()
}
