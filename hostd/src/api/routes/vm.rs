use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{delete, get, post, put},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    api::server::AppState,
    common::vm::{EnvVar, NetworkConfig, VmConfig, VmMode, VmState},
};

use super::network::{self};

/// VM routes, to be nested under `/vms`.
pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list_vms).post(create_vm))
        .route("/{id}", get(get_vm).put(update_vm).delete(delete_vm))
        .nest("/{id}/network", network::routes())
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub(crate) struct VmResponse {
    pub id: String,
    pub status: VmState,
    pub config: VmConfig,
}

/// Stub: create a new vm.
pub(crate) async fn create_vm(
    State(state): State<AppState>,
    Json(payload): Json<VmConfig>,
) -> (StatusCode, Json<Value>) {
    let vm_id = state.vmm.create_vm(&payload).await.ok(); // TODO: handle errors
    (
        StatusCode::CREATED,
        Json(json!({ "status": "created", "payload": payload, "id": vm_id })),
    )
}

/// Stub: list all vms.
pub(crate) async fn list_vms(State(_state): State<AppState>) -> Json<Value> {
    let vms: Vec<VmResponse> = vec![];
    Json(json!({ "status": "not implemented", "vms": vms }))
}

/// Stub: get a single vm by id.
pub(crate) async fn get_vm(State(_state): State<AppState>, Path(id): Path<String>) -> Json<Value> {
    let vm = VmResponse::default();
    Json(json!({ "status": "not implemented", "vm": vm }))
}

/// Stub: update a vm by id.
pub(crate) async fn update_vm(
    State(_state): State<AppState>,
    Path(id): Path<String>,
    Json(payload): Json<VmConfig>,
) -> Json<Value> {
    let vm = VmResponse::default();
    Json(json!({ "status": "not implemented", "vm": vm }))
}

/// Stub: delete a vm by id.
pub(crate) async fn delete_vm(
    State(_state): State<AppState>,
    Path(id): Path<String>,
) -> StatusCode {
    tracing::debug!(%id, "delete_vm stub called");
    StatusCode::NO_CONTENT
}
