//! Tests-only checkpoint v2: MCP envelope bound on full-context reads.
//!
//! Supersedes the v1 envelope file (preserved in git history, NOT amended)
//! after nine review defects. Still tests-only, unwired, no Cargo/GH/runtime.
//!
//! WIRING (root applies):
//! ```ignore
//! #[cfg(all(test, unix))]
//! #[path = "context_envelope_tests.rs"]
//! mod envelope_tests;
//! ```
//! (`target_os = "unix"` is INVALID as a cfg string — `unix` alone is the
//! correct cfg.) Placement beside `lib_tests.rs` (crate root visibility for
//! `crate::handle_request_for_actor`). NOT compiled or run here — predictions
//! below; root names actual RED.
//!
//! CONTRACT UNDER TEST: the WHOLE `tools/call` jsonrpc response for a
//! read-only full-context read is bounded by ENVELOPE_BUDGET_BYTES
//! (3 MiB = 3,145,728 bytes, binary). Small reads ship both representations
//! unchanged; oversized reads return the small typed failure (code
//! `context_too_large` in `structuredContent.error`, fixed summary/task_list
//! recovery pointers, < 4 KiB whole). The bound covers read-only full
//! context ONLY — mutating tools, post-send contexts, and the `id` echo are
//! explicitly out of it, and no executed write is ever reclassified.
//!
//! PREDICTIONS on 655fe75f (daemon bound present, NO envelope bound):
//! RED = fitting-2.2MB-envelope, nested-fitting-envelope (both ~2× over
//! budget unmarked); GREEN-or-pinned = oversized-typed (error path already
//! small — pins it), normal/archived, large-send, huge-id, nested-small,
//! nested-oversized. A GREEN-where-RED-predicted result means the fixture is
//! wrong, not the contract.

use cccc_client::DaemonClient;
use serde_json::{Map, Value, json};

/// 3 MiB WHOLE-response bound (3,145,728 bytes). Fixture ceiling AND
/// contract number coincide here by design.
const ENVELOPE_BUDGET_BYTES: usize = 3 * 1024 * 1024;
/// Small-error ceiling for typed refusals.
const ERROR_BUDGET_BYTES: usize = 4 * 1024;

const ACTOR: &str = "peer1";

struct LiveDaemon {
    _temp: tempfile::TempDir,
    home: cccc_core::HomeLayout,
    client: DaemonClient,
    group_id: String,
    daemon: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl Drop for LiveDaemon {
    fn drop(&mut self) {
        // Abort, never join: a stuck daemon must not hang teardown.
        self.daemon.abort();
    }
}

impl LiveDaemon {
    async fn start() -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = cccc_core::HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = cccc_core::GroupStore::new(home.clone()).expect("store");
        let created = store.create("envelope", "").expect("group");
        let mut group = store.load(&created.group_id).expect("reload group");
        cccc_core::actors::add(&mut group, cccc_contracts::Actor::new(ACTOR)).expect("add actor");
        store.save(&group).expect("persist group WITH actor");
        let persisted = store.load(&created.group_id).expect("verify load");
        assert!(
            persisted.actors.iter().any(|actor| actor.id == ACTOR),
            "actor must actually persist (mutate+save the SAME doc)"
        );
        let daemon_home = home.clone();
        let daemon = tokio::spawn(async move { cccc_daemon::run(daemon_home).await });
        // Short per-call timeout: readiness polling must fail in seconds,
        // never stack 60 s defaults into hours.
        let client =
            DaemonClient::new(home.clone()).with_timeout(std::time::Duration::from_secs(5));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            let ready = client
                .call(&cccc_contracts::DaemonRequest {
                    v: 1,
                    op: "group_list".into(),
                    args: Map::new(),
                })
                .await
                .is_ok();
            if ready {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "daemon never became ready"
            );
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        Self {
            _temp: temp,
            home,
            client,
            group_id: created.group_id,
            daemon,
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

    async fn capability_context_full(&self, id: Value) -> Value {
        crate::handle_request_for_actor(
            &self.home,
            &json!({"id":id,"method":"tools/call","params":{"name":"cccc_capability_use",
                "arguments":{"tool_name":"cccc_context_get","reason":"envelope scope probe",
                "tool_arguments":{"detail":"full"}}}}),
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

    /// Classify FIRST: tool-level failure (isError + exact code) vs success.
    /// Never branch on size first — a small typed error must not be asked to
    /// prove giant tasks.
    fn error_code(response: &Value) -> Option<&str> {
        if response["result"]["isError"].as_bool() != Some(true) {
            return None;
        }
        Some(
            response["result"]["structuredContent"]["error"]["code"]
                .as_str()
                .unwrap_or(""),
        )
    }
}

#[tokio::test]
async fn fitting_daemon_result_envelope_bounded_or_typed() {
    // 2,200,000 decimal bytes of notes (~2.1 MiB): daemon success under its
    // bound, but pretty text + structuredContent ≈ 4,400,000+ bytes whole
    // (duplication dominates; pretty overhead on string-heavy payloads is
    // near zero — all math pre-run estimates). PREDICTION: RED.
    let daemon = LiveDaemon::start().await;
    daemon
        .sync(vec![
            json!({"op":"task.create","title":"big","notes":"x".repeat(2_200_000)}),
        ])
        .await;
    let response = daemon.context_full(json!(1), Map::new()).await;
    let bytes = LiveDaemon::whole_bytes(&response);
    match LiveDaemon::error_code(&response) {
        Some("context_too_large") => {
            assert!(
                bytes <= ERROR_BUDGET_BYTES,
                "typed refusal itself stays small"
            );
            let text = response.to_string();
            assert!(
                !text.contains(&"x".repeat(1024)),
                "no giant duplicate in refusal"
            );
            assert!(text.contains("task_list"), "fixed recovery pointers");
        }
        Some(other) => panic!("unexpected tool error: {other}"),
        None => {
            assert!(
                bytes <= ENVELOPE_BUDGET_BYTES,
                "unmarked envelope {bytes} bytes over budget"
            );
            let text = response["result"]["content"][0]["text"]
                .as_str()
                .expect("pretty text present");
            let parsed: Value = serde_json::from_str(text).expect("pretty text parses");
            assert_eq!(
                parsed, response["result"]["structuredContent"],
                "text == structured"
            );
            let tasks = parsed["coordination"]["tasks"]
                .as_array()
                .expect("canonical tasks");
            assert_eq!(tasks.len(), 1, "no silent drop or invented task");
            assert!(
                tasks[0]["notes"].as_str() == Some("x".repeat(2_200_000).as_str()),
                "canonical notes unchanged"
            );
            assert!(
                parsed["coordination"]["tasks"]
                    .as_array()
                    .is_some_and(|tasks| tasks.iter().any(|t| t["title"] == "big"))
            );
        }
    }
}

#[tokio::test]
async fn oversized_envelope_returns_typed_error() {
    // 4,500,000-byte notes: daemon bound refuses; the MCP error mapping must
    // stay small with code + pointers. Likely GREEN on 655 — pins it either way.
    let daemon = LiveDaemon::start().await;
    daemon
        .sync(vec![
            json!({"op":"task.create","title":"giant","notes":"x".repeat(4_500_000)}),
        ])
        .await;
    let response = daemon.context_full(json!(2), Map::new()).await;
    let bytes = LiveDaemon::whole_bytes(&response);
    assert_eq!(LiveDaemon::error_code(&response), Some("context_too_large"));
    assert!(
        bytes <= ERROR_BUDGET_BYTES,
        "error response {bytes} bytes over 4 KiB"
    );
    assert!(
        response.to_string().contains("task_list"),
        "recovery pointers"
    );
    assert!(
        !response.to_string().contains(&"x".repeat(1024)),
        "no giant duplicate"
    );
}

#[tokio::test]
async fn nested_fitting_envelope_bounded_or_typed() {
    // Outer metadata + re-envelope around a fitting inner read can exceed
    // budget even when the inner result fits — direct coverage does NOT
    // stand for the wrapper. PREDICTION: RED (giant nested envelope).
    let daemon = LiveDaemon::start().await;
    daemon
        .sync(vec![
            json!({"op":"task.create","title":"big","notes":"x".repeat(2_200_000)}),
        ])
        .await;
    let response = daemon.capability_context_full(json!(3)).await;
    let bytes = LiveDaemon::whole_bytes(&response);
    match LiveDaemon::error_code(&response) {
        Some("context_too_large") => {
            assert!(bytes <= ERROR_BUDGET_BYTES, "nested refusal stays small");
        }
        Some(other) => panic!("unexpected nested tool error: {other}"),
        None => {
            assert!(
                bytes <= ENVELOPE_BUDGET_BYTES,
                "nested unmarked envelope {bytes} bytes over budget"
            );
            let structured = &response["result"]["structuredContent"];
            let text: Value = serde_json::from_str(
                response["result"]["content"][0]["text"]
                    .as_str()
                    .expect("nested text"),
            )
            .expect("nested text JSON");
            assert_eq!(&text, structured, "both nested representations unchanged");
            let tasks = structured["tool_result"]["coordination"]["tasks"]
                .as_array()
                .expect("nested canonical tasks");
            assert_eq!(tasks.len(), 1);
            assert!(
                tasks[0]["notes"].as_str() == Some("x".repeat(2_200_000).as_str()),
                "nested notes unchanged"
            );
        }
    }
}

#[tokio::test]
async fn nested_small_envelope_intact_preserved() {
    // GREEN now and after: small nested reads ship both envelopes unchanged,
    // inner result present under tool_result.
    let daemon = LiveDaemon::start().await;
    daemon
        .sync(vec![json!({"op":"task.create","title":"tiny"})])
        .await;
    let response = daemon.capability_context_full(json!(10)).await;
    assert!(LiveDaemon::error_code(&response).is_none());
    let inner = &response["result"]["structuredContent"]["tool_result"];
    assert!(inner.is_object(), "inner result present under tool_result");
    assert!(inner.to_string().contains("tiny"));
}

#[tokio::test]
async fn nested_oversized_envelope_returns_typed_error() {
    // Giant group through the wrapper: the inner typed error propagates
    // without giant nested duplication. Likely GREEN on 655 — pins it.
    let daemon = LiveDaemon::start().await;
    daemon
        .sync(vec![
            json!({"op":"task.create","title":"giant","notes":"x".repeat(4_500_000)}),
        ])
        .await;
    let response = daemon.capability_context_full(json!(11)).await;
    let bytes = LiveDaemon::whole_bytes(&response);
    assert_eq!(LiveDaemon::error_code(&response), Some("context_too_large"));
    assert!(
        bytes <= ERROR_BUDGET_BYTES,
        "nested error {bytes} bytes over 4 KiB"
    );
    assert!(
        !response.to_string().contains(&"x".repeat(1024)),
        "no giant nested duplicate"
    );
}

#[tokio::test]
async fn normal_full_and_archived_filter_preserved() {
    // GREEN now and after. Canonical tasks in BOTH representations; archived
    // excluded by default.
    let daemon = LiveDaemon::start().await;
    daemon
        .sync(vec![
            json!({"op":"task.create","title":"live","notes":"keep"}),
            json!({"op":"task.create","title":"old","status":"archived"}),
        ])
        .await;
    let response = daemon.context_full(json!(4), Map::new()).await;
    assert!(LiveDaemon::error_code(&response).is_none());
    let structured = &response["result"]["structuredContent"];
    let tasks = structured["coordination"]["tasks"]
        .as_array()
        .expect("tasks");
    assert!(
        tasks
            .iter()
            .any(|t| t["title"] == "live" && t["notes"] == "keep")
    );
    assert!(
        !tasks.iter().any(|t| t["title"] == "old"),
        "archived filtered by default"
    );
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(text.contains("live"), "pretty text carries the task");
    let parsed: Value = serde_json::from_str(text).expect("pretty text parses");
    assert_eq!(
        parsed["coordination"]["tasks"],
        structured["coordination"]["tasks"]
    );
}

#[tokio::test]
async fn large_mutating_send_succeeds_unreclassified() {
    // A 2,000,000-byte send proves write paths are uncapped AND never
    // reclassified: success with no isError. (A 100 KiB send could never
    // exercise a 3 MiB exemption.) GREEN pin.
    let daemon = LiveDaemon::start().await;
    // Self-only actor mail is deliberately rejected by audience policy.
    // Use a second enabled actor so this control exercises a valid write.
    let store = cccc_core::GroupStore::new(daemon.home.clone()).expect("store");
    let mut group = store.load(&daemon.group_id).expect("group");
    cccc_core::actors::add(&mut group, cccc_contracts::Actor::new("peer2")).expect("add recipient");
    store.save(&group).expect("persist enabled recipient");
    let response = crate::handle_request_for_actor(
        &daemon.home,
        &json!({"id":5,"method":"tools/call","params":{"name":"cccc_message_send",
            "arguments":{"to":["peer2"],"insight":"envelope scope probe",
                         "text":"y".repeat(2_000_000),"mode":"mail"}}}),
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
        "no failure reclassification: {}",
        response["result"]["structuredContent"]["error"]
    );
    assert!(
        LiveDaemon::whole_bytes(&response) > ENVELOPE_BUDGET_BYTES,
        "fixture must exercise an actually oversized write response"
    );
    assert!(
        response["result"]["structuredContent"]["event"]["data"]["text"].as_str()
            == Some("y".repeat(2_000_000).as_str()),
        "the write event retained its full text"
    );
}

#[tokio::test]
async fn oversized_rpc_id_echoed_without_content_duplication() {
    // The bound measures RESULT payload, never the id: a 4,000,000-byte id
    // echoes exactly on success; on failure the id echoes WITHOUT megabytes
    // of content attached. GREEN pins of both halves.
    let daemon = LiveDaemon::start().await;
    daemon
        .sync(vec![json!({"op":"task.create","title":"a"})])
        .await;
    let big_id = json!("z".repeat(4_000_000));
    let ok_response = daemon.context_full(big_id.clone(), Map::new()).await;
    assert!(LiveDaemon::error_code(&ok_response).is_none());
    assert!(
        ok_response["id"] == big_id,
        "id echoed verbatim, never truncated"
    );
    daemon
        .sync(vec![
            json!({"op":"task.create","title":"giant","notes":"x".repeat(4_500_000)}),
        ])
        .await;
    let fail_response = daemon.context_full(big_id.clone(), Map::new()).await;
    assert!(fail_response["id"] == big_id, "id kept on failure too");
    assert_eq!(
        LiveDaemon::error_code(&fail_response),
        Some("context_too_large")
    );
    let without_id = {
        let mut clone = fail_response.clone();
        clone["id"] = json!(0);
        clone
    };
    assert!(
        LiveDaemon::whole_bytes(&without_id) <= ERROR_BUDGET_BYTES,
        "failure carries the id but no content megabytes"
    );
}
