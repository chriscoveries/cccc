// Harness RAM over the last 72 hours, and the Group's health right now.
//
// Read only: the console shows the figures, nothing acts on them.
use axum::Router;
use axum::extract::{Path, State};
use axum::routing::get;
use serde_json::json;

use crate::AppState;
use crate::api::{ApiResult, call, object};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/groups/{group_id}/actor_memory", get(memory_report))
        .route("/api/v1/groups/{group_id}/health", get(health))
}

async fn memory_report(State(state): State<AppState>, Path(group_id): Path<String>) -> ApiResult {
    call(
        &state,
        "actor_memory_report",
        object(json!({"group_id":group_id})),
    )
    .await
}

// The numbers behind the header's running chip and its pulldown. The console
// polls this every ten seconds while the header is open.
async fn health(State(state): State<AppState>, Path(group_id): Path<String>) -> ApiResult {
    call(&state, "group_health", object(json!({"group_id":group_id}))).await
}
