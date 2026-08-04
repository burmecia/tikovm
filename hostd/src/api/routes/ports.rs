//! Exposed-port routes: the per-VM registry of guest ports (with labels) that
//! HTTP workloads listen on.
//!
//! The registry lives in `VmConfig.network_config.exposed_ports` and is
//! VM-side metadata only — nothing on the host forwards traffic to these
//! ports yet. Host-side reachability (a JWT-authenticated reverse proxy) is a
//! planned follow-up that will read this registry.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::get,
};

use crate::{
    api::{
        error::{ApiJson, ApiResult},
        server::AppState,
    },
    net::ExposedPort,
    vmm::vm::VmId,
};

/// Exposed-port routes, to be nested under `/vms/{id}/ports`.
pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list_exposed_ports).post(add_exposed_port))
        .route("/{port}", axum::routing::delete(remove_exposed_port))
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
