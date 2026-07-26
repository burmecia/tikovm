use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
};
use serde_json::{Value, json};

use crate::{
    api::{
        error::{ApiJson, ApiResult},
        server::AppState,
    },
    common::vm::{VmConfig, VmId, VmInstance, VmSnapshot},
    error::Error,
};

use super::network::{self};

/// VM routes, to be nested under `/vms`.
pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list_vms).post(create_vm))
        .route("/{id}", get(get_vm).put(update_vm).delete(delete_vm))
        .route("/{id}/pause", post(pause_vm))
        .route("/{id}/resume", post(resume_vm))
        .route("/{id}/snapshot", post(snapshot_vm))
        .route("/{id}/restore", post(restore_vm))
        .nest("/{id}/network", network::routes())
}

/// Stub: create a new vm.
pub(crate) async fn create_vm(
    State(state): State<AppState>,
    ApiJson(payload): ApiJson<VmConfig>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let vm_id = state.vmm.create_vm(&payload).await?;
    state.vmm.start_vm(&vm_id).await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "status": "created", "payload": payload, "id": vm_id })),
    ))
}

/// List all vms.
pub(crate) async fn list_vms(State(state): State<AppState>) -> ApiResult<Json<Vec<VmInstance>>> {
    let instance_refs = state.vmm.list_vms().await?;
    let mut vms = Vec::with_capacity(instance_refs.len());
    for instance_ref in instance_refs {
        let vm_instance = instance_ref.lock()?;
        vms.push(vm_instance.clone());
    }
    Ok(Json(vms))
}

/// Get a single vm by id.
pub(crate) async fn get_vm(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<VmInstance>> {
    let vm_instance_ref = state
        .vmm
        .get_vm(&VmId(id.clone()))
        .await?
        .ok_or(Error::VmNotFound(id))?;
    let vm_instance = vm_instance_ref.lock()?;
    Ok(Json(vm_instance.clone()))
}

/// Stub: update a vm by id.
pub(crate) async fn update_vm(
    State(_state): State<AppState>,
    Path(_id): Path<String>,
    ApiJson(_payload): ApiJson<VmConfig>,
) -> Json<Value> {
    Json(json!({ "status": "not implemented" }))
}

/// Pause a vm by id.
pub(crate) async fn pause_vm(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<VmInstance>> {
    state.vmm.pause_vm(&VmId(id.clone())).await?;
    let vm_instance_ref = state
        .vmm
        .get_vm(&VmId(id.clone()))
        .await?
        .ok_or(Error::VmNotFound(id))?;
    let vm_instance = vm_instance_ref.lock()?;
    Ok(Json(vm_instance.clone()))
}

/// Resume a paused vm by id.
pub(crate) async fn resume_vm(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<VmInstance>> {
    state.vmm.resume_vm(&VmId(id.clone())).await?;
    let vm_instance_ref = state
        .vmm
        .get_vm(&VmId(id.clone()))
        .await?
        .ok_or(Error::VmNotFound(id))?;
    let vm_instance = vm_instance_ref.lock()?;
    Ok(Json(vm_instance.clone()))
}

/// Take a snapshot of a running vm by id, leaving it suspended.
pub(crate) async fn snapshot_vm(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<VmSnapshot>> {
    let snapshot = state.vmm.snapshot_vm(&VmId(id)).await?;
    Ok(Json(snapshot))
}

/// Restore a suspended vm from its snapshot.
pub(crate) async fn restore_vm(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<VmInstance>> {
    state.vmm.restore_vm(&VmId(id.clone())).await?;
    let vm_instance_ref = state
        .vmm
        .get_vm(&VmId(id.clone()))
        .await?
        .ok_or(Error::VmNotFound(id))?;
    let vm_instance = vm_instance_ref.lock()?;
    Ok(Json(vm_instance.clone()))
}

/// Delete a vm by id.
pub(crate) async fn delete_vm(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    state.vmm.destroy_vm(&VmId(id)).await?;
    Ok(StatusCode::NO_CONTENT)
}
