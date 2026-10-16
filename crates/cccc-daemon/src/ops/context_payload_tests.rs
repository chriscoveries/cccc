//! Tests-only checkpoint v2: bounded `context_get detail=full` (train001).
//!
//! Supersedes the v1 checkpoint file (preserved in git history, NOT amended)
//! after six concrete review defects: OpResult is `Result<Map, OpError>`
//! (not Value); owned-`Value` request args; exact test counts; no empty-Ok
//! acceptance; fixture math that actually exceeds budget; a genuine
//! dedupe RED plus error-size and field-level preservation pins.
//!
//! WIRING (root applies — UNWIRED; no existing source touched):
//! ```ignore
//! #[cfg(test)]
//! #[path = "context_payload_tests.rs"]
//! mod payload_tests;
//! ```
//! in `crates/cccc-daemon/src/ops/context.rs`. All tests drive the REAL
//! `super::get` dispatch with real `DaemonRequest`s over a seeded
//! `ContextStore` (pattern mirrors `task_list_tests.rs`). NOT compiled or
//! run here — predictions below; root names actual RED.
//!
//! CONTRACT UNDER TEST: serialized full responses are bounded by
//! FULL_BUDGET_BYTES (3 MiB = 3,145,728). Reads that cannot fit return typed
//! `context_too_large` with fixed recovery instructions, never silent
//! partials. `version`, `tasks_version`, existing error codes unchanged.
//!
//! PREDICTIONS vs e1ada4e0: `*_requires_typed_error` + `many_medium_*` +
//! `dedupe_*` FAIL (unbounded multi-copy Ok); `normal_full_versions` +
//! `summary_overview_errors` PASS now and must stay green.

use super::*;
use cccc_contracts::DaemonRequest;
use cccc_core::{GroupStore, HomeLayout, context::ContextStore};
use serde_json::{Map, Value, json};

/// Proposed bound: 3 MiB serialized response bytes (3,145,728 — binary).
/// The implementation must define the same value; these tests pin the NUMBER.
const FULL_BUDGET_BYTES: usize = 3 * 1024 * 1024;
/// Oversized single card: 4,500,000 notes bytes exceeds 3 MiB alone AND
/// 4 MiB binary (4,194,304), so the bound decision cannot hide behind unit
/// ambiguity and no dedupe shape can fit it.
const GIANT_NOTES_BYTES: usize = 4_500_000;
/// Medium field: 80 × 24 KiB canonical notes = 1,966,080 bytes fit alone,
/// while TWO mandatory full copies (coordination.tasks + board buckets)
/// reach 3,932,160 > budget regardless of the attention subset.
const MEDIUM_COUNT: usize = 80;
const MEDIUM_NOTES_BYTES: usize = 24 * 1024;

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

fn response_bytes(result: &Map<String, Value>) -> usize {
    serde_json::to_vec(result)
        .map(|bytes| bytes.len())
        .unwrap_or(usize::MAX)
}

/// Typed-error requirement for fixtures that CANNOT fit: full unchanged
/// content provably exceeds budget, so only `context_too_large` is
/// acceptable — never a huge Ok, never an empty Ok. The error itself must
/// stay small, serialized whole (code + message + details).
fn require_typed_too_large(
    outcome: Result<Map<String, Value>, crate::dispatch::OpError>,
    via: &str,
) {
    match outcome {
        Ok(result) => {
            panic!(
                "{via}: full unchanged content cannot fit yet returned Ok ({} bytes)",
                response_bytes(&result)
            );
        }
        Err(error) => {
            assert_eq!(error.code, "context_too_large", "{via}: wrong error code");
            let rendered = serde_json::to_vec(
                &json!({"code": error.code, "message": error.message, "details": error.details}),
            )
            .expect("error serializes");
            assert!(
                rendered.len() < 4096,
                "{via}: oversized error must stay small"
            );
            let message = String::from_utf8_lossy(&rendered);
            for pointer in ["summary", "task_list"] {
                assert!(
                    message.contains(pointer),
                    "{via}: recovery instructions must name {pointer}"
                );
            }
        }
    }
}

#[test]
fn oversized_single_card_requires_typed_error() {
    // Canonical single copy (4,500,000 notes bytes) already exceeds budget —
    // no dedupe shape can fit it. PREDICTION: RED (two full copies here).
    let needed = GIANT_NOTES_BYTES;
    assert!(
        needed > FULL_BUDGET_BYTES,
        "fixture must exceed budget in ONE copy"
    );
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
    require_typed_too_large(outcome, "giant card");
}

#[test]
fn many_medium_tasks_fit_only_deduped() {
    // Canonical input fits (1,966,080) but TWO mandatory full copies
    // (coordination.tasks + board buckets) reach 3,932,160 > budget whatever
    // the attention subset holds. Assert the math BEFORE the op.
    // PREDICTION: RED (≈3.9 MB+ Ok: 2 full copies + attention subset).
    let canonical_min = MEDIUM_COUNT * MEDIUM_NOTES_BYTES;
    assert!(
        canonical_min < FULL_BUDGET_BYTES,
        "canonical input itself fits"
    );
    assert!(
        2 * canonical_min > FULL_BUDGET_BYTES,
        "two copies must exceed budget"
    );
    let (_temp, home, group_id) = seeded_home();
    let ops: Vec<Map<String, Value>> = (0..MEDIUM_COUNT)
        .map(|index| {
            op(json!({
                "op": "task.create",
                "title": format!("medium {index:02}"),
                "notes": "y".repeat(MEDIUM_NOTES_BYTES),
                "waiting_on": if index % 3 == 0 { "user" } else { "none" },
                "handoff_to": if index % 5 == 0 { "peer" } else { "" },
            }))
        })
        .collect();
    sync_ops(&home, &group_id, ops);
    let result =
        super::get(&home, &request(&group_id, json!({"detail":"full"}))).expect("medium full");
    let bytes = response_bytes(&result);
    assert!(
        bytes <= FULL_BUDGET_BYTES,
        "deduped medium board {bytes} bytes over budget"
    );
    // Content verification against seeded values — resolvability alone could
    // pass on an emptied response. Canonical tasks complete, count and IDs
    // exact, notes byte-exact. Order-independent: task numbering follows
    // creation order, but the contract under test is content, not position.
    let canonical = result["coordination"]["tasks"]
        .as_array()
        .expect("canonical tasks");
    assert_eq!(canonical.len(), MEDIUM_COUNT, "no task dropped or invented");
    let mut seen = std::collections::BTreeSet::new();
    let mut ids = std::collections::BTreeSet::new();
    for task in canonical {
        let title = task["title"].as_str().expect("title").to_owned();
        let id = task["id"].as_str().expect("id").to_owned();
        assert_eq!(
            task["notes"],
            "y".repeat(MEDIUM_NOTES_BYTES),
            "{title} notes byte-exact"
        );
        assert!(seen.insert(title.clone()), "duplicate title {title}");
        assert!(ids.insert(id.clone()), "duplicate id {id}");
    }
    for index in 0..MEDIUM_COUNT {
        assert!(
            seen.contains(&format!("medium {index:02}")),
            "missing medium {index:02}"
        );
    }
    let want_ids: std::collections::BTreeSet<String> =
        (1..=MEDIUM_COUNT).map(|n| format!("T{n:03}")).collect();
    assert_eq!(ids, want_ids, "canonical IDs exact");
    assert!(
        result.get("truncated").is_none(),
        "fitting board carries no markers"
    );
}

#[test]
fn giant_non_task_fields_require_typed_error() {
    // agent_states/meta/brief/decisions ship whole in EVERY detail level, so
    // no task-shape dedupe can save them: 3.5 MiB constructed > budget.
    // PREDICTION: RED (multi-MB Ok).
    let constructed: usize = 1024 * 1024 + 1024 * 1024 + 512 * 1024 + 1024 * 1024;
    assert!(
        constructed > FULL_BUDGET_BYTES,
        "fixture must exceed budget"
    );
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
    require_typed_too_large(outcome, "giant non-task fields");
}

const LIGHTWEIGHT_REF_FIELDS: [&str; 2] = ["id", "status"];

fn assert_lightweight(entry: &Value, via: &str) {
    let object = entry.as_object().expect("ref object");
    for key in object.keys() {
        assert!(
            LIGHTWEIGHT_REF_FIELDS.contains(&key.as_str()),
            "{via}: ref carries undeclared field {key}"
        );
    }
    assert!(
        entry.get("id").and_then(Value::as_str).is_some(),
        "{via}: ref needs id"
    );
    assert!(
        entry.get("status").and_then(Value::as_str).is_some(),
        "{via}: MCP archived filtering needs the referenced status"
    );
    assert!(
        entry.get("notes").is_none(),
        "{via}: ref must not carry notes"
    );
    assert!(
        entry.get("checklist").is_none(),
        "{via}: ref must not carry checklist"
    );
}

#[test]
fn dedupe_refs_lightweight_canonical_complete() {
    // Genuine dedupe RED: canonical coordination.tasks stay COMPLETE while
    // every attention/board entry carries ONLY declared lightweight ref
    // fields. Fails on current full-object copies; no shape-agnostic pass
    // is accepted as the dedupe oracle (that lives in normal_full_versions).
    // PREDICTION: RED (board/attention carry notes/checklist/custom blobs).
    let (_temp, home, group_id) = seeded_home();
    sync_ops(
        &home,
        &group_id,
        vec![
            op(
                json!({"op":"task.create","title":"a","notes":"small notes here",
                      "checklist":[{"id":"c1","text":"measure","status":"done"}],
                      "waiting_on":"user","custom_blob":{"x":1}}),
            ),
            op(json!({"op":"task.create","title":"b","handoff_to":"peer"})),
            op(json!({"op":"task.create","title":"c"})),
        ],
    );
    let result =
        super::get(&home, &request(&group_id, json!({"detail":"full"}))).expect("normal full");
    let canonical = result["coordination"]["tasks"]
        .as_array()
        .expect("canonical");
    assert_eq!(canonical.len(), 3);
    let first = canonical
        .iter()
        .find(|task| task["title"] == "a")
        .expect("task a canonical");
    assert_eq!(first["notes"], "small notes here", "canonical notes intact");
    assert_eq!(first["checklist"][0]["text"], "measure");
    assert_eq!(
        first["custom_blob"]["x"], 1,
        "canonical custom fields intact"
    );
    let expected_ids: std::collections::BTreeSet<&str> = canonical
        .iter()
        .map(|task| task["id"].as_str().expect("canonical id"))
        .collect();
    let planned = result["board"]["planned"].as_array().expect("planned refs");
    let actual_ids: std::collections::BTreeSet<&str> = planned
        .iter()
        .map(|task| task["id"].as_str().expect("board ref id"))
        .collect();
    assert_eq!(
        planned.len(),
        canonical.len(),
        "every planned task is represented once"
    );
    assert_eq!(
        actual_ids, expected_ids,
        "board cannot silently drop a reference"
    );
    for entry in planned {
        assert_eq!(entry["status"], "planned");
    }
    assert_eq!(result["attention"]["blocked"], json!([]));
    assert_eq!(
        result["attention"]["waiting_user"]
            .as_array()
            .expect("waiting refs")
            .len(),
        1
    );
    assert_eq!(result["attention"]["waiting_user"][0]["id"], first["id"]);
    let handoff = canonical
        .iter()
        .find(|task| task["title"] == "b")
        .expect("task b");
    assert_eq!(
        result["attention"]["pending_handoffs"]
            .as_array()
            .expect("handoff refs")
            .len(),
        1
    );
    assert_eq!(
        result["attention"]["pending_handoffs"][0]["id"],
        handoff["id"]
    );
    let mut refs = 0usize;
    for section in ["blocked", "waiting_user", "pending_handoffs"] {
        if let Some(entries) = result["attention"][section].as_array() {
            for entry in entries {
                assert_lightweight(entry, "attention");
                refs += 1;
            }
        }
    }
    for bucket in ["planned", "active", "done", "archived"] {
        if let Some(entries) = result["board"][bucket].as_array() {
            for entry in entries {
                assert_lightweight(entry, "board");
                refs += 1;
            }
        }
    }
    assert!(refs > 0, "fixture must exercise at least one ref path");
}

#[test]
fn normal_full_versions_preserved() {
    // Preservation: versions, error-shape stability, no markers on fitting
    // boards. GREEN on e1ada4e0 and after.
    let (_temp, home, group_id) = seeded_home();
    sync_ops(
        &home,
        &group_id,
        vec![op(json!({"op":"task.create","title":"a"}))],
    );
    let contexts = ContextStore::new(home.clone()).expect("contexts");
    let saved = contexts.load(&group_id).expect("saved context");
    let expected_version = contexts.version(&saved).expect("saved version");
    let result =
        super::get(&home, &request(&group_id, json!({"detail":"full"}))).expect("normal full");
    assert!(result.get("version").and_then(Value::as_str).is_some());
    assert!(
        result
            .get("tasks_version")
            .and_then(Value::as_str)
            .is_some()
    );
    assert_eq!(result["version"], expected_version);
    assert_eq!(
        result["tasks_version"],
        format!("tasksv:{}", saved.tasks_revision)
    );
    assert!(result.get("truncated").is_none());
    assert_eq!(
        result["coordination"]["tasks"]
            .as_array()
            .expect("tasks")
            .len(),
        1
    );
}

#[test]
fn summary_overview_errors_preserved() {
    // Field-level preservation (not just board absence): summary keeps the
    // 18-field task shape incl. notes; overview keeps coordination notes and
    // omits task collections. Error codes pinned. GREEN on both.
    let (_temp, home, group_id) = seeded_home();
    sync_ops(
        &home,
        &group_id,
        vec![op(json!({"op":"task.create","title":"a","notes":"keep me",
                        "checklist":[{"id":"c1","text":"m","status":"done"}]}))],
    );
    let summary =
        super::get(&home, &request(&group_id, json!({"detail":"summary"}))).expect("summary");
    assert!(summary.get("board").is_none());
    let task = &summary["coordination"]["tasks"][0];
    assert_eq!(
        task["notes"], "keep me",
        "summary keeps notes (18-field shape)"
    );
    assert_eq!(task["checklist"][0]["text"], "m");
    assert_eq!(summary["attention"]["blocked"], 0);
    let overview =
        super::get(&home, &request(&group_id, json!({"detail":"overview"}))).expect("overview");
    assert!(overview.get("board").is_none());
    assert!(overview["coordination"].get("tasks").is_none());
    let bad = super::get(&home, &request(&group_id, json!({"detail":"bogus"}))).unwrap_err();
    assert_eq!(bad.code, "invalid_detail");
    let missing = super::get(&home, &request("g_missing", json!({"detail":"full"}))).unwrap_err();
    assert_eq!(missing.code, "group_not_found");
}
