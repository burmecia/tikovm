use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{delete, get, post, put},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    api::{
        error::{ApiJson, ApiResult},
        server::AppState,
    },
    common::vm::{EnvVar, NetworkConfig, VmConfig, VmId, VmMode, VmState},
    error::Error,
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
    ApiJson(payload): ApiJson<VmConfig>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let vm_id = state.vmm.create_vm(&payload).await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "status": "created", "payload": payload, "id": vm_id })),
    ))
}

/// Stub: list all vms.
pub(crate) async fn list_vms(State(_state): State<AppState>) -> Json<Value> {
    let vms: Vec<VmResponse> = vec![];
    Json(json!({ "status": "not implemented", "vms": vms }))
}

/// Get a single vm by id.
pub(crate) async fn get_vm(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<VmResponse>> {
    let vm_instance_ref = state
        .vmm
        .get_vm(&VmId(id.clone()))
        .await?
        .ok_or(Error::VmNotFound(id))?;
    let vm_instance = vm_instance_ref.lock()?;
    Ok(Json(VmResponse {
        id: vm_instance.vm_id.to_string(),
        status: vm_instance.state,
        config: vm_instance.vm_config.clone(),
    }))
}

/// Stub: update a vm by id.
pub(crate) async fn update_vm(
    State(_state): State<AppState>,
    Path(id): Path<String>,
    ApiJson(payload): ApiJson<VmConfig>,
) -> Json<Value> {
    let vm = VmResponse::default();
    Json(json!({ "status": "not implemented", "vm": vm }))
}

/// Delete a vm by id.
pub(crate) async fn delete_vm(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    state.vmm.destroy_vm(&VmId(id)).await?;
    Ok(StatusCode::NO_CONTENT)
}
