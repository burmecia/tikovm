//! Exposed-port routes: the per-VM registry of guest ports (with labels) that
//! HTTP workloads listen on, plus minting of the ephemeral JWTs the proxy
//! server uses to authenticate forwarded requests.
//!
//! The registry lives in `VmConfig.network_config.exposed_ports`; the proxy
//! (`hostd/src/proxy/`) re-validates it on every forwarded request, so
//! removing a port here revokes proxy access immediately.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    api::{
        error::{ApiJson, ApiResult},
        server::AppState,
    },
    error::Error,
    net::ExposedPort,
    proxy::{DEFAULT_TTL_SECS, Proto},
    vmm::vm::VmId,
};

/// Exposed-port routes, to be nested under `/vms/{id}/ports`.
pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list_exposed_ports).post(add_exposed_port))
        .route("/{port}", axum::routing::delete(remove_exposed_port))
        .route("/{port}/token", post(mint_port_token))
}

/// List a vm's exposed ports.
pub(crate) async fn list_exposed_ports(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<ExposedPort>>> {
    Ok(Json(state.vmm.list_exposed_ports(&VmId(id)).await?))
}

/// Expose a guest port, with a label describing its purpose.
pub(crate) async fn add_exposed_port(
    State(state): State<AppState>,
    Path(id): Path<String>,
    ApiJson(port): ApiJson<ExposedPort>,
) -> ApiResult<(StatusCode, Json<ExposedPort>)> {
    let port = state.vmm.add_exposed_port(&VmId(id), port).await?;
    Ok((StatusCode::CREATED, Json(port)))
}

/// Remove an exposed port by port number.
pub(crate) async fn remove_exposed_port(
    State(state): State<AppState>,
    Path((id, port)): Path<(String, u16)>,
) -> ApiResult<StatusCode> {
    state.vmm.remove_exposed_port(&VmId(id), port).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub(crate) struct MintTokenRequest {
    /// Requested token lifetime in seconds; defaults to
    /// [`DEFAULT_TTL_SECS`] and is clamped to the proxy's maximum.
    #[serde(default)]
    ttl_secs: Option<u64>,
    /// Forwarding mode the token is valid for: `http` (default) or `tcp`
    /// (e.g. the Postgres wire protocol, see `proxy/tcp.rs`).
    #[serde(default)]
    proto: Option<Proto>,
}

#[derive(Debug, Serialize)]
pub(crate) struct MintTokenResponse {
    token: String,
    expires_at: DateTime<Utc>,
}

/// Mint an ephemeral JWT authorizing proxy requests to this exposed port.
pub(crate) async fn mint_port_token(
    State(state): State<AppState>,
    Path((id, port)): Path<(String, u16)>,
    ApiJson(body): ApiJson<MintTokenRequest>,
) -> ApiResult<(StatusCode, Json<MintTokenResponse>)> {
    // Minting requires the port to be exposed right now; the proxy
    // re-checks this on every forwarded request as well.
    let vm_id = VmId(id);
    let ports = state.vmm.list_exposed_ports(&vm_id).await?;
    if !ports.iter().any(|p| p.port == port) {
        return Err(Error::PortNotExposed {
            vm_id: vm_id.to_string(),
            port,
        }
        .into());
    }
    let (token, expires_at) = state.tokens.mint(
        &vm_id,
        port,
        body.proto.unwrap_or(Proto::Http),
        body.ttl_secs.unwrap_or(DEFAULT_TTL_SECS),
    )?;
    Ok((
        StatusCode::CREATED,
        Json(MintTokenResponse { token, expires_at }),
    ))
}
