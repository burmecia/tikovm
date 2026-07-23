use axum::{Json, Router, extract::Path, routing::get};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Default)]
pub(crate) struct NetworkConfig {
    pub allow_internet: bool,
    #[serde(default)]
    pub ingress_ports: Vec<u16>,
    #[serde(default)]
    pub egress: Vec<String>,
    #[serde(default)]
    pub public_access: bool,
}

/// Network routes, to be nested under `/vms/{id}/network`.
pub(crate) fn routes() -> Router {
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
    Json(payload): Json<NetworkConfig>,
) -> Json<NetworkConfig> {
    tracing::debug!(%vm_id, "update_network stub called");
    Json(payload)
}
