use axum::{Json, Router, extract::Path, routing::get};

use crate::{
    api::{error::ApiJson, server::AppState},
    common::vm::NetworkConfig,
};

/// Network routes, to be nested under `/vms/{id}/network`.
pub(crate) fn routes() -> Router<AppState> {
    Router::new().route("/", get(get_network).put(update_network))
}

/// Stub: get the network config for a vm.
pub(crate) async fn get_network(Path(vm_id): Path<String>) -> Json<NetworkConfig> {
    tracing::debug!(%vm_id, "get_network stub called");
    Json(NetworkConfig::default())
}

/// Stub: update the network config for a vm.
pub(crate) async fn update_network(
    Path(vm_id): Path<String>,
    ApiJson(payload): ApiJson<NetworkConfig>,
) -> Json<NetworkConfig> {
    tracing::debug!(%vm_id, "update_network stub called");
    Json(payload)
}
