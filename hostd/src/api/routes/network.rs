//! Stub network routes: wired into the router so the paths exist, but the
//! handlers are placeholders — `get_network` returns the default config
//! regardless of the VM and `update_network` echoes the payload without
//! persisting anything.

use axum::{Json, Router, extract::Path, routing::get};

use crate::{
    api::{error::ApiJson, server::AppState},
    net::NetworkConfig,
};

/// Network routes, to be nested under `/vms/{id}/network`.
pub(crate) fn routes() -> Router<AppState> {
    Router::new().route("/", get(get_network).put(update_network))
}

/// Stub: get the network config for a vm.
async fn get_network(Path(vm_id): Path<String>) -> Json<NetworkConfig> {
    tracing::debug!(%vm_id, "get_network stub called");
    Json(NetworkConfig::default())
}

/// Stub: update the network config for a vm.
async fn update_network(
    Path(vm_id): Path<String>,
    ApiJson(payload): ApiJson<NetworkConfig>,
) -> Json<NetworkConfig> {
    tracing::debug!(%vm_id, "update_network stub called");
    Json(payload)
}
