use axum::{Json, Router, extract::Path, http::StatusCode, routing::get};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::network::{self, NetworkConfig};

/// VM routes, to be nested under `/vms`.
pub(crate) fn routes() -> Router {
    Router::new()
        .route("/", get(list_vms).post(create_vm))
        .route("/{id}", get(get_vm).put(update_vm).delete(delete_vm))
        .nest("/{id}/network", network::routes())
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct CreateVmRequest {
    pub name: String,
    pub image: String,
    pub project: String,
    pub mode: VmMode,
    pub config: VmConfig,
}

#[derive(Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub(crate) enum VmMode {
    #[default]
    Ephemeral,
    Permanent,
    Schedule,
}

#[derive(Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub(crate) enum VmStatus {
    #[default]
    Creating,
    Created,
    Starting,
    Started,
    Pausing,
    Paused,
    Suspending,
    Suspended,
    Restoring,
    Destroying,
    Destroyed,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub(crate) struct VmConfig {
    pub cpus: u32,
    pub memory_mb: u32,
    pub disk_size_mb: u32,
    pub network_config: NetworkConfig,
    pub ssh_access: bool,
    #[serde(default)]
    pub env: Vec<EnvVar>,
    #[serde(default)]
    pub cmd: Vec<String>,
    #[serde(default)]
    pub services: Vec<String>,
    #[serde(default)]
    pub cron_schedule: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct EnvVar {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub(crate) struct Vm {
    pub id: String,
    pub name: String,
    pub image: String,
    pub project: String,
    pub mode: VmMode,
    pub status: VmStatus,
    pub config: VmConfig,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Stub: create a new vm.
// Example payload:
// {
//   "name": "my-app",
//   "image": "node-20",
//   "project": "production",
//   "mode": "ephemeral",   // "ephemeral" or "permanent" or "schedule"
//   "status": "running" | "stopped" | "paused" | "creating" | ...
//   "config": {
//     "cpus": 2,
//     "memory_mb": 2048,
//     "disk_size_mb": 1024,
//     "network_config": {
//       "allow_internet": true,
//       "ingress_ports": [80, 443, 3000],
//       "egress": ["api.github.com", "registry.npmjs.org"],
//       "public_access": false
//     },
//     "ssh_access": true,
//     "env": [
//       { "key": "NODE_ENV", "value": "production" },
//       { "key": "PORT", "value": "3000" }
//     ],
//     "cmd": ["node server.js"],
//     "services": ["redis", "postgres"],
//     "cron_schedule": "0 0 * * *"
//   },
//   "tags": ["web", "api"]
// }
pub(crate) async fn create_vm(Json(payload): Json<CreateVmRequest>) -> (StatusCode, Json<Value>) {
    (
        StatusCode::CREATED,
        Json(json!({ "status": "not implemented", "payload": payload })),
    )
}

/// Stub: list all vms.
pub(crate) async fn list_vms() -> Json<Value> {
    let vms: Vec<Vm> = vec![];
    Json(json!({ "status": "not implemented", "vms": vms }))
}

/// Stub: get a single vm by id.
pub(crate) async fn get_vm(Path(id): Path<String>) -> Json<Value> {
    let vm = Vm {
        id,
        ..Default::default()
    };
    Json(json!({ "status": "not implemented", "vm": vm }))
}

/// Stub: update a vm by id.
pub(crate) async fn update_vm(
    Path(id): Path<String>,
    Json(payload): Json<CreateVmRequest>,
) -> Json<Value> {
    let vm = Vm {
        id,
        name: payload.name,
        image: payload.image,
        project: payload.project,
        mode: payload.mode,
        status: VmStatus::Created,
        config: payload.config,
        tags: Vec::new(),
    };
    Json(json!({ "status": "not implemented", "vm": vm }))
}

/// Stub: delete a vm by id.
pub(crate) async fn delete_vm(Path(id): Path<String>) -> StatusCode {
    tracing::debug!(%id, "delete_vm stub called");
    StatusCode::NO_CONTENT
}
