// Harness RAM over the last 72 hours, as the daemon folds it.
//
// Read only: the console shows the trend, nothing acts on it.
use axum::Router;
use axum::extract::{Path, State};
use axum::routing::get;
use serde_json::json;

use crate::AppState;
use crate::api::{ApiResult, call, object};

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/v1/groups/{group_id}/actor_memory", get(memory_report))
}

async fn memory_report(State(state): State<AppState>, Path(group_id): Path<String>) -> ApiResult {
    call(
        &state,
        "actor_memory_report",
        object(json!({"group_id":group_id})),
    )
    .await
}
