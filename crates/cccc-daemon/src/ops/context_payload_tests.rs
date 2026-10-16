//! Tests-only checkpoint: bounded `context_get detail=full` (train001 follow-up).
//!
//! WIRING (root applies — this file is intentionally UNWIRED; no existing
//! source was touched for this checkpoint). Append to the end of
//! `crates/cccc-daemon/src/ops/context.rs`:
//! ```ignore
//! #[cfg(test)]
//! #[path = "context_payload_tests.rs"]
//! mod payload_tests;
//! ```
//! All tests drive the REAL `super::get` dispatch with real `DaemonRequest`s
//! over a seeded `ContextStore` (same pattern as `task_list_tests.rs`).
//! NOT compiled or run here — predictions below; root names actual RED.
//!
//! CONTRACT UNDER TEST (separate statement, not code): serialized full
//! responses are bounded by FULL_BUDGET_BYTES (3 MiB = 3,145,728). A full
//! read that cannot fit returns typed `context_too_large` with fixed
//! recovery instructions (use `summary`/`overview` + `task_list`), never a
//! silent partial context. `version`, `tasks_version`, and all existing
//! error codes are unchanged on every path.
//!
//! PREDICTIONS vs e1ada4e0 (unpatched): *_bounded_or_typed tests FAIL (Ok
//! with multi-MB payload, no bound, no typed error); *_preserved and the
//! resolvability halves PASS now and must stay green after the fix.

use super::*;
use cccc_contracts::DaemonRequest;
use cccc_core::{GroupStore, HomeLayout, context::ContextStore};
use serde_json::{Map, Value, json};

/// Proposed bound: 3 MiB serialized response bytes. The implementation must
/// define the same value (suggested home: beside `MAX_PAGE_SIZE`-style
/// constants); these tests pin the NUMBER, not the item path.
const FULL_BUDGET_BYTES: usize = 3 * 1024 * 1024;
/// Oversized single card: larger than even a loose 4 MiB reading, so the
/// bound decision cannot hide behind unit ambiguity (4 MiB = 4,194,304).
const GIANT_NOTES_BYTES: usize = 4_500_000;

fn request(group_id: &str, args: Value) -> DaemonRequest {
    let mut args = args.as_object().cloned().expect("args");
    args.insert("group_id".into(), json!(group_id));
    DaemonRequest {
        v: 1,
        op: "context_get".into(),
        args,
    }
}

fn seeded_home() -> (tempfile::TempDir, HomeLayout, String) {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    let group = GroupStore::new(home.clone())
        .expect("store")
        .create("payload", "")
        .expect("group")
        .group_id;
    (temp, home, group)
}

fn sync_ops(home: &HomeLayout, group_id: &str, ops: Vec<Map<String, Value>>) {
    ContextStore::new(home.clone())
        .expect("contexts")
        .sync(group_id, &ops, None, "user", false)
        .expect("seed");
}

fn op(value: Value) -> Map<String, Value> {
    value.as_object().cloned().expect("op object")
}

fn full_bytes(result: &Value) -> usize {
    serde_json::to_vec(result).map(|bytes| bytes.len()).unwrap_or(usize::MAX)
}

/// The bound-or-typed contract: EITHER a bounded success OR a small typed
/// error. Asserts NEVER a huge unmarked success. `via` names the path.
fn assert_bounded_or_typed(
    outcome: Result<Value, crate::dispatch::OpError>,
    via: &str,
) {
    match outcome {
        Ok(result) => {
            let bytes = full_bytes(&result);
            assert!(
                bytes <= FULL_BUDGET_BYTES,
                "{via}: full response {bytes} bytes exceeds {FULL_BUDGET_BYTES} without a typed error"
            );
            assert!(
                result.get("truncated").is_none(),
                "{via}: unmarked partial context is never acceptable"
            );
        }
        Err(error) => {
            assert_eq!(error.code, "context_too_large", "{via}: wrong error code");
            assert!(
                error.message.len() < 4096,
                "{via}: oversized error must stay small"
            );
            for pointer in ["summary", "task_list"] {
                assert!(
                    error.message.contains(pointer),
                    "{via}: recovery instructions must name {pointer}"
                );
            }
        }
    }
}

#[test]
fn oversized_single_card_bounded_or_typed() {
    // PREDICTION: RED on e1ada4e0 (≈13 MB Ok: 4.5 MB notes × 3 full copies).
    let (_temp, home, group_id) = seeded_home();
    sync_ops(
        &home,
        &group_id,
        vec![
            op(json!({"op":"task.create","title":"giant","notes":"x".repeat(GIANT_NOTES_BYTES)})),
            op(json!({"op":"task.create","title":"small"})),
        ],
    );
    let outcome = super::get(&home, &request(&group_id, json!({"detail":"full"})));
    assert_bounded_or_typed(outcome, "giant card");
}

#[test]
fn many_medium_tasks_bounded_or_typed() {
    // PREDICTION: RED on e1ada4e0 (≈3.6 MB+: 60 × 20 KiB × 3 copies).
    let (_temp, home, group_id) = seeded_home();
    let mut ops: Vec<Map<String, Value>> = (0..60)
        .map(|index| {
            op(json!({
                "op": "task.create",
                "title": format!("medium {index:02}"),
                "notes": "y".repeat(20 * 1024),
                "waiting_on": if index % 3 == 0 { "user" } else { "none" },
                "handoff_to": if index % 5 == 0 { "peer" } else { "" },
            }))
        })
        .collect();
    ops.push(op(json!({"op":"task.create","title":"blocked one","blocked_by":["T001"]})));
    sync_ops(&home, &group_id, ops);
    let outcome = super::get(&home, &request(&group_id, json!({"detail":"full"})));
    assert_bounded_or_typed(outcome, "many medium");
}

#[test]
fn giant_non_task_fields_bounded_or_typed() {
    // agent_states/meta/brief/decisions ship whole in EVERY detail level.
    // PREDICTION: RED on e1ada4e0 (≈4 MB+ Ok).
    let (_temp, home, group_id) = seeded_home();
    sync_ops(
        &home,
        &group_id,
        vec![
            op(json!({"op":"task.create","title":"anchor"})),
            op(json!({
                "op": "agent_state.update", "actor_id": "worker",
                "focus": "z".repeat(1024 * 1024),
                "open_loops": ["w".repeat(1024 * 1024)],
            })),
            op(json!({
                "op": "coordination.brief.update",
                "project_brief": "v".repeat(1024 * 1024),
            })),
            op(json!({
                "op": "coordination.note.add", "kind": "decision",
                "summary": "u".repeat(512 * 1024),
            })),
            op(json!({
                "op": "meta.merge",
                "data": {"project_status": "t".repeat(1024 * 1024)},
            })),
        ],
    );
    let outcome = super::get(&home, &request(&group_id, json!({"detail":"full"})));
    assert_bounded_or_typed(outcome, "giant non-task fields");
}

fn resolve_in_canonical(tasks: &Value, id: &str) -> bool {
    tasks.as_array().is_some_and(|items| {
        items.iter().any(|task| task.get("id").and_then(Value::as_str) == Some(id))
    })
}

/// Every attention/board entry must EITHER be a full object whose id appears
/// in canonical tasks, OR an id-ref resolving there. Accepts BOTH the current
/// full-object shape AND the proposed ref shape — green now, green after.
fn assert_resolvable(result: &Value, via: &str) {
    let canonical = &result["coordination"]["tasks"];
    assert!(canonical.is_array(), "{via}: canonical tasks array present");
    let mut checked = 0usize;
    for section in ["blocked", "waiting_user", "pending_handoffs"] {
        if let Some(entries) = result["attention"][section].as_array() {
            for entry in entries {
                let id = entry.get("id").and_then(Value::as_str).expect("entry id");
                assert!(resolve_in_canonical(canonical, id), "{via}: attention ref {id} resolves");
                checked += 1;
            }
        }
    }
    for bucket in ["planned", "active", "done", "archived"] {
        if let Some(entries) = result["board"][bucket].as_array() {
            for entry in entries {
                let id = entry.get("id").and_then(Value::as_str).expect("entry id");
                assert!(resolve_in_canonical(canonical, id), "{via}: board ref {id} resolves");
                checked += 1;
            }
        }
    }
    assert!(checked > 0, "{via}: fixture must exercise at least one ref path");
}

#[test]
fn normal_full_resolvable_and_versioned() {
    // PREDICTION: GREEN on e1ada4e0 (full objects resolve trivially) AND
    // after the fix (id-refs resolve in canonical tasks). Pins the invariant
    // across the migration instead of either shape.
    let (_temp, home, group_id) = seeded_home();
    sync_ops(
        &home,
        &group_id,
        vec![
            op(json!({"op":"task.create","title":"a","notes":"small","waiting_on":"user"})),
            op(json!({"op":"task.create","title":"b","handoff_to":"peer"})),
            op(json!({"op":"task.create","title":"c"})),
        ],
    );
    let result =
        super::get(&home, &request(&group_id, json!({"detail":"full"}))).expect("normal full");
    assert_eq!(result["coordination"]["tasks"][0]["notes"], "small");
    assert_resolvable(&result, "normal full");
    assert!(result.get("version").and_then(Value::as_str).is_some());
    assert!(result.get("tasks_version").and_then(Value::as_str).is_some());
    assert!(result.get("truncated").is_none());
}

#[test]
fn summary_overview_and_errors_preserved() {
    // PREDICTION: GREEN on e1ada4e0 and after (no collateral from the fix).
    let (_temp, home, group_id) = seeded_home();
    sync_ops(&home, &group_id, vec![op(json!({"op":"task.create","title":"a"}))]);
    let summary = super::get(&home, &request(&group_id, json!({"detail":"summary"})))
        .expect("summary");
    assert!(summary.get("board").is_none());
    let overview = super::get(&home, &request(&group_id, json!({"detail":"overview"})))
        .expect("overview");
    assert!(overview.get("board").is_none());
    let bad = super::get(&home, &request(&group_id, json!({"detail":"bogus"}))).unwrap_err();
    assert_eq!(bad.code, "invalid_detail");
    let missing =
        super::get(&home, &request("g_missing", &json!({"detail":"full"}))).unwrap_err();
    assert_eq!(missing.code, "group_not_found");
}
