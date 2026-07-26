use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
};

use crate::{
    api::{
        error::{ApiJson, ApiResult},
        server::AppState,
    },
    common::{
        vm::VmId,
        workload::{Workload, WorkloadId, WorkloadLogEntry, WorkloadSpec},
    },
};

/// Workload routes, to be nested under `/vms/{id}/workloads`.
pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list_workloads).post(start_workload))
        .route("/{workload_id}", get(get_workload))
        .route("/{workload_id}/stop", post(stop_workload))
        .route("/{workload_id}/logs", get(workload_logs))
}

/// Start a new workload: run a command inside the vm via guestd.
pub(crate) async fn start_workload(
    State(state): State<AppState>,
    Path(id): Path<String>,
    ApiJson(spec): ApiJson<WorkloadSpec>,
) -> ApiResult<(StatusCode, Json<Workload>)> {
    let workload = state.vmm.start_workload(&VmId(id), spec).await?;
    Ok((StatusCode::CREATED, Json(workload)))
}

/// List all workloads of a vm.
pub(crate) async fn list_workloads(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<Workload>>> {
    Ok(Json(state.vmm.list_workloads(&VmId(id)).await?))
}

/// Get a workload's status and run result.
pub(crate) async fn get_workload(
    State(state): State<AppState>,
    Path((id, workload_id)): Path<(String, String)>,
) -> ApiResult<Json<Workload>> {
    Ok(Json(
        state
            .vmm
            .get_workload(&VmId(id), &WorkloadId(workload_id))
            .await?,
    ))
}

/// Stop a running workload (SIGTERM, escalating to SIGKILL in the guest).
pub(crate) async fn stop_workload(
    State(state): State<AppState>,
    Path((id, workload_id)): Path<(String, String)>,
) -> ApiResult<Json<Workload>> {
    Ok(Json(
        state
            .vmm
            .stop_workload(&VmId(id), &WorkloadId(workload_id))
            .await?,
    ))
}

/// Get a workload's captured stdout/stderr, in arrival order.
pub(crate) async fn workload_logs(
    State(state): State<AppState>,
    Path((id, workload_id)): Path<(String, String)>,
) -> ApiResult<Json<Vec<WorkloadLogEntry>>> {
    Ok(Json(
        state
            .vmm
            .workload_logs(&VmId(id), &WorkloadId(workload_id))
            .await?,
    ))
}
