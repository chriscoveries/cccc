//! Tests-only checkpoint: MCP envelope bound on full-context reads.
//!
//! WIRING (root applies — UNWIRED; no existing source touched):
//! ```ignore
//! #[cfg(all(test, target_os = "unix"))]
//! #[path = "context_envelope_tests.rs"]
//! mod envelope_tests;
//! ```
//! in `crates/cccc-mcp/src/lib_tests.rs` (or equivalent test root with
//! `crate::` visibility). NOT compiled or run here — predictions below;
//! root names actual RED.
//!
//! CONTRACT UNDER TEST (this file DEFINES it, tests-first): the WHOLE
//! `tools/call` JSONRPC response for a read-only full-context read is
//! bounded by ENVELOPE_BUDGET_BYTES (3 MiB = 3,145,728, serialized). Both
//! representations ship unchanged when small (pretty `content.text` AND
//! `structuredContent` — measured AFTER all post-mutation hooks, never
//! `tool_result` alone, since `with_post_message_context` rebuilds text and
//! injects `mail_pending`). When the full-read envelope cannot fit, return
//! the small typed `context_too_large` failure with fixed summary/task_list
//! recovery (never a silent full-payload omission, never a giant duplicate).
//! The bound covers read-only full context ONLY — mutating tools and
//! post-send contexts are out of scope, and no already-executed write is
//! ever reclassified as failed.
//!
//! HARNESS (existing precedent `lib_tests.rs`): temp home, REAL spawned
//! `cccc_daemon::run` + REAL `DaemonClient`, seed via REAL `context_sync`
//! dispatch, measure through the REAL consumer entrypoint
//! `crate::handle_request_for_actor` (not `tool_result` alone). The nested
//! `cccc_capability_use` path shares the same envelope shape
//! (`call_with_context` → `tool_result`); entrypoint coverage here stands
//! for both, noted not duplicated.
//!
//! PREDICTIONS on 655fe75f (daemon bound implemented, NO envelope bound):
//! `fitting_daemon_result_envelope_bounded_or_typed` FAILS (~2.2 MB daemon
//! result → ~4.7 MB whole response unmarked); `oversized_envelope_typed`
//! FAILS if the error mapping duplicates content, else passes (see body);
//! the other three PASS now and must stay green. A GREEN-where-RED-
//! predicted result means the fixture is wrong, not the contract.

use cccc_client::DaemonClient;
use serde_json::{json, Map, Value};

/// Proposed bound: 3 MiB WHOLE jsonrpc response bytes (3,145,728, binary).
/// Fixture ceiling AND contract number are the same here by design: the
/// envelope has no legitimate content beyond both representations.
const ENVELOPE_BUDGET_BYTES: usize = 3 * 1024 * 1024;
/// Small-error ceiling: typed failures stay an order of magnitude under.
const ERROR_BUDGET_BYTES: usize = 4 * 1024;

const ACTOR: &str = "peer1";

struct LiveDaemon {
    _temp: tempfile::TempDir,
    home: cccc_core::HomeLayout,
    client: DaemonClient,
    group_id: String,
    _daemon: tokio::task::JoinHandle<()>,
}

impl LiveDaemon {
    async fn start() -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = cccc_core::HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = cccc_core::GroupStore::new(home.clone()).expect("store");
        let group = store.create("envelope", "").expect("group");
        cccc_core::actors::add(
            &mut store.load(&group.group_id).expect("load"),
            cccc_contracts::Actor::new(ACTOR),
        )
        .expect("add actor");
        store
            .save(&store.load(&group.group_id).expect("reload"))
            .expect("save group");
        let daemon_home = home.clone();
        let daemon = tokio::spawn(async move { cccc_daemon::run(daemon_home).await });
        let client = DaemonClient::new(home.clone());
        for _ in 0..200 {
            if client
                .call(&cccc_contracts::DaemonRequest {
                    v: 1,
                    op: "group_list".into(),
                    args: Map::new(),
                })
                .await
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        Self {
            _temp: temp,
            home,
            client,
            group_id: group.group_id,
            _daemon: daemon,
        }
    }

    async fn sync(&self, ops: Vec<Value>) {
        let ops: Vec<Map<String, Value>> = ops
            .into_iter()
            .map(|op| op.as_object().cloned().expect("op object"))
            .collect();
        let seeded = self
            .client
            .call(&cccc_contracts::DaemonRequest {
                v: 1,
                op: "context_sync".into(),
                args: Map::from_iter([
                    ("group_id".into(), json!(self.group_id)),
                    (
                        "ops".into(),
                        Value::Array(ops.into_iter().map(Value::Object).collect()),
                    ),
                ]),
            })
            .await
            .expect("seed dispatch");
        assert!(seeded.ok, "seed ops applied: {seeded:?}");
    }

    async fn context_full(&self, id: Value, extra: Map<String, Value>) -> Value {
        let mut arguments = Map::from_iter([("detail".into(), json!("full"))]);
        arguments.extend(extra);
        crate::handle_request_for_actor(
            &self.home,
            &json!({"id":id,"method":"tools/call","params":{"name":"cccc_context_get","arguments":arguments}}),
            &self.group_id,
            ACTOR,
        )
        .await
    }

    fn whole_bytes(response: &Value) -> usize {
        serde_json::to_vec(response)
            .map(|bytes| bytes.len())
            .unwrap_or(usize::MAX)
    }
}

fn is_typed_too_large(response: &Value) -> bool {
    let text = response.to_string();
    text.contains("context_too_large") && text.contains("task_list")
}

#[tokio::test]
async fn fitting_daemon_result_envelope_bounded_or_typed() {
    // ~2.2 MB notes → daemon success under its 3 MiB bound, but pretty text
    // (~2.5 MB) + structuredContent (~2.2 MB) ≈ 4.7 MB whole response.
    // PREDICTION: RED — 4.7 MB unmarked Ok today.
    let daemon = LiveDaemon::start().await;
    daemon
        .sync(vec![
            json!({"op":"task.create","title":"big","notes":"x".repeat(2_200_000)}),
        ])
        .await;
    let response = daemon.context_full(json!(1), Map::new()).await;
    let bytes = LiveDaemon::whole_bytes(&response);
    if bytes <= ENVELOPE_BUDGET_BYTES {
        // Fits: both representations ship unchanged (no silent slimming).
        let text = response["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default();
        assert!(text.contains("big"), "pretty text intact when small");
        assert!(
            response["result"]["structuredContent"]["coordination"]["tasks"]
                .as_array()
                .is_some_and(|tasks| tasks.iter().any(|t| t["title"] == "big"))
        );
    } else {
        // Cannot fit: ONLY a small typed refusal with fixed recovery.
        assert!(
            is_typed_too_large(&response),
            "oversized envelope must be typed, got {bytes} bytes"
        );
        let error_text = response.to_string();
        assert!(
            !error_text.contains(&"x".repeat(1024)),
            "no giant content duplicate in refusal"
        );
        assert!(bytes <= 64 * 1024, "typed refusal itself stays small");
    }
}

#[tokio::test]
async fn oversized_envelope_returns_typed_error() {
    // 4.5 MB notes: daemon bound refuses first; the MCP error mapping must
    // stay small and carry recovery pointers. PREDICTION: likely GREEN on
    // 655 (error path carries code/message only) — pins it either way.
    let daemon = LiveDaemon::start().await;
    daemon
        .sync(vec![
            json!({"op":"task.create","title":"giant","notes":"x".repeat(4_500_000)}),
        ])
        .await;
    let response = daemon.context_full(json!(2), Map::new()).await;
    let bytes = LiveDaemon::whole_bytes(&response);
    assert!(
        bytes <= ERROR_BUDGET_BYTES,
        "error response {bytes} bytes over 4 KiB"
    );
    assert!(
        is_typed_too_large(&response),
        "typed code + recovery pointers"
    );
    assert!(
        !response.to_string().contains(&"x".repeat(1024)),
        "no giant duplicate"
    );
}

#[tokio::test]
async fn normal_full_and_archived_filter_preserved() {
    // PREDICTION: GREEN now and after. Full canonical tasks in BOTH
    // representations; archived excluded by default; error shape stable.
    let daemon = LiveDaemon::start().await;
    daemon
        .sync(vec![
            json!({"op":"task.create","title":"live","notes":"keep"}),
            json!({"op":"task.create","title":"old","status":"archived"}),
        ])
        .await;
    let response = daemon.context_full(json!(3), Map::new()).await;
    assert!(response.get("error").is_none(), "normal read succeeds");
    let structured = &response["result"]["structuredContent"];
    let tasks = structured["coordination"]["tasks"]
        .as_array()
        .expect("tasks");
    assert!(tasks
        .iter()
        .any(|t| t["title"] == "live" && t["notes"] == "keep"));
    assert!(
        !tasks.iter().any(|t| t["title"] == "old"),
        "archived filtered by default"
    );
    assert!(response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .contains("live"));
}

#[tokio::test]
async fn mutating_tools_unaffected_by_read_bound() {
    // The bound covers READ-ONLY full context: a large send must still
    // succeed and must never be reclassified as failed. GREEN pin.
    let daemon = LiveDaemon::start().await;
    let response = crate::handle_request_for_actor(
        &daemon.home,
        &json!({"id":4,"method":"tools/call","params":{"name":"cccc_message_send",
            "arguments":{"to":[ACTOR],"insight":"envelope scope probe",
                         "text":"y".repeat(100_000)}}}),
        &daemon.group_id,
        ACTOR,
    )
    .await;
    assert!(
        response.get("error").is_none(),
        "large send succeeds: {response}"
    );
    assert!(
        response["result"].get("isError").is_none(),
        "no tool-level failure reclassification: {response}"
    );
}

#[tokio::test]
async fn arbitrary_rpc_id_echoed_verbatim() {
    // The bound measures RESULT payload, never the id: a huge id echoes
    // exactly (distinction from result bounding, asserted — not bounded).
    let daemon = LiveDaemon::start().await;
    let big_id = json!("z".repeat(100_000));
    let response = daemon.context_full(big_id.clone(), Map::new()).await;
    assert_eq!(
        response["id"], big_id,
        "id echoed verbatim, never truncated"
    );
}
