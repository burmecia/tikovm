use std::time::Duration;

use axum::{
    Json,
    extract::{Path, State},
};
use serde::Serialize;

use crate::{
    api::{
        error::{ApiJson, ApiResult},
        server::AppState,
    },
    error::Error,
    vmm::{
        vm::VmId,
        workload::{Workload, WorkloadLogEntry, WorkloadSpec},
    },
};

/// How long exec waits for the command to finish before stopping it and
/// failing the request, so a hung command cannot hang the HTTP connection
/// forever.
const EXEC_TIMEOUT: Duration = Duration::from_secs(300);

/// How often exec polls the workload state: get_workload is a cheap
/// in-memory read and half-second granularity is plenty for command
/// execution.
const EXEC_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Exec response: the finished workload plus its captured logs.
#[derive(Debug, Serialize)]
pub(crate) struct ExecResponse {
    #[serde(flatten)]
    workload: Workload,
    logs: Vec<WorkloadLogEntry>,
}

/// Run a command inside the vm and block until it exits: a synchronous
/// wrapper over the workload APIs (start -> poll terminal state -> logs).
/// The workload stays registered and inspectable via the workloads API.
pub(crate) async fn exec_command(
    State(state): State<AppState>,
    Path(id): Path<String>,
    ApiJson(spec): ApiJson<WorkloadSpec>,
) -> ApiResult<Json<ExecResponse>> {
    let vm_id = VmId(id);
    // Wake the VM first if it is auto-suspended; a no-op when running.
    state.vmm.ensure_started(&vm_id).await?;
    let workload = state.vmm.start_workload(&vm_id, spec).await?;
    let workload_id = workload.workload_id.clone();

    let deadline = tokio::time::Instant::now() + EXEC_TIMEOUT;
    let workload = loop {
        let workload = state.vmm.get_workload(&vm_id, &workload_id).await?;
        if !workload.is_active() {
            break workload;
        }
        if tokio::time::Instant::now() >= deadline {
            // Best-effort stop so the command doesn't keep running in the
            // guest after the request fails.
            let _ = state.vmm.stop_workload(&vm_id, &workload_id).await;
            return Err(Error::vmm(format!(
                "exec of workload {workload_id} timed out after {}s",
                EXEC_TIMEOUT.as_secs()
            ))
            .into());
        }
        tokio::time::sleep(EXEC_POLL_INTERVAL).await;
    };

    let logs = state.vmm.workload_logs(&vm_id, &workload_id).await?;
    Ok(Json(ExecResponse { workload, logs }))
}
