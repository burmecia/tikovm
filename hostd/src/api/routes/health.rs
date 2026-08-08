use axum::Json;
use serde_json::{Value, json};

/// Health check endpoint.
pub(crate) async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}
