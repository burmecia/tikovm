//! Network config routes: read the VM's live [`NetworkConfig`] (which the
//! `/ports` endpoints mutate). There is intentionally no update route:
//! `exposed_ports` is managed via `/vms/{id}/ports`, and the remaining
//! fields (`allow_internet`, `egress`, `public_access`) are not enforced
//! anywhere yet, so accepting writes would pretend otherwise.

use axum::{Json, Router, extract::Path, extract::State, routing::get};

use crate::{
    api::{error::ApiResult, server::AppState},
    error::Error,
    net::NetworkConfig,
    vmm::vm::VmId,
};

/// Network routes, nested under `/vms/{id}/network`.
pub(crate) fn routes() -> Router<AppState> {
    Router::new().route("/", get(get_network))
}

/// Get the network config of a VM.
async fn get_network(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<NetworkConfig>> {
    let instance_ref = state
        .vmm
        .get_vm(&VmId(id.clone()))
        .await?
        .ok_or(Error::VmNotFound(id))?;
    let config = instance_ref.lock()?.vm_config.network_config.clone();
    Ok(Json(config))
}
