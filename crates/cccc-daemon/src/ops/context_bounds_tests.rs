//! Bounded-context regression tests.
//!
//! The fixture is a verbatim capture of the cwflab group's context document
//! taken from the live daemon on 2026-10-19 (775 real task cards, 43 real agent
//! states). Its unbounded projection is ~4.3 MB, which is the payload that made
//! `cccc_context_get` fail and retry until the DSH sessions corrupted. See
//! `tests/fixtures/context/README.md` for the capture command.

use super::*;
use cccc_contracts::DaemonRequest;
use cccc_core::GroupDoc;
use serde_json::{Map, Value, json};
use std::io::Read;

const FIXTURE: &[u8] =
    include_bytes!("../../tests/fixtures/context/real-context-g1a9e39785e1a.zip");

fn real_document() -> ContextDoc {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(FIXTURE)).expect("fixture archive");
    let mut file = archive.by_index(0).expect("fixture entry");
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).expect("fixture bytes");
    serde_json::from_slice(&bytes).expect("fixture document")
}

fn request(args: Value) -> DaemonRequest {
    DaemonRequest {
        v: 1,
        op: "context_get".into(),
        args: args.as_object().expect("object args").clone(),
    }
}

fn group_with(extra: Value) -> GroupDoc {
    let mut base = Map::from_iter([
        ("v".into(), json!(1)),
        ("group_id".into(), json!("g_real")),
        ("title".into(), json!("cwflab")),
        ("created_at".into(), json!("2026-10-19T00:00:00Z")),
        ("updated_at".into(), json!("2026-10-19T00:00:00Z")),
    ]);
    for (key, value) in extra.as_object().cloned().unwrap_or_default() {
        base.insert(key, value);
    }
    serde_json::from_value(Value::Object(base)).expect("group document")
}

fn bounds(args: Value, extra: Value) -> Bounds {
    Bounds::from_request(&request(args), &group_with(extra))
        .expect("valid bounds")
        .expect("bounded")
}

fn size(value: &Value) -> usize {
    serde_json::to_vec(value).expect("serialized").len()
}

fn task_ids(value: &Value) -> Vec<String> {
    value
        .pointer("/coordination/tasks")
        .and_then(Value::as_array)
        .expect("task page")
        .iter()
        .filter_map(|task| task.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect()
}

fn actor_ids(value: &Value) -> Vec<String> {
    value
        .get("agent_states")
        .and_then(Value::as_array)
        .expect("actor page")
        .iter()
        .filter_map(|state| state.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect()
}

#[test]
fn the_captured_document_is_the_incident_payload() {
    let document = real_document();
    assert_eq!(document.tasks.len(), 775);
    assert_eq!(document.agent_states.len(), 43);
    let unbounded =
        super::super::context_projection::project(document, "ctxv:17189".into(), "full");
    // The live daemon returned 4,497,303 bytes for this group on 2026-10-19.
    assert!(
        size(&unbounded) > 4_000_000,
        "fixture must reproduce the multi-megabyte answer, got {}",
        size(&unbounded)
    );
}

#[test]
fn default_answer_is_truncated_under_the_byte_cap() {
    let document = real_document();
    let bounds = bounds(json!({}), json!({}));
    assert_eq!(bounds.limit, DEFAULT_LIMIT);
    let answer = project(&document, "ctxv:17189", "full", &bounds);
    assert!(
        size(&answer) <= DEFAULT_STATE_MAX_BYTES,
        "{}",
        size(&answer)
    );
    assert_eq!(answer["truncated"], json!(true));
    // 775 captured cards, 142 archived: a default page counts the 633 live ones.
    assert_eq!(answer["paging"]["task_total"], json!(633));
    assert!(task_ids(&answer).len() <= DEFAULT_LIMIT);
    assert!(answer["next_cursor"].as_str().is_some());
}

#[test]
fn paging_walks_every_real_task_and_actor_exactly_once() {
    let document = real_document();
    for include_archived in [false, true] {
        let mut cursor: Option<String> = None;
        let mut seen_tasks: Vec<String> = Vec::new();
        let mut seen_actors: Vec<String> = Vec::new();
        let mut pages = 0;
        loop {
            let mut args = json!({"limit": 40, "include_archived": include_archived});
            if let Some(cursor) = &cursor {
                args["cursor"] = json!(cursor);
            }
            let answer = project(&document, "ctxv:17189", "full", &bounds(args, json!({})));
            assert!(size(&answer) <= DEFAULT_STATE_MAX_BYTES);
            seen_tasks.extend(task_ids(&answer));
            seen_actors.extend(actor_ids(&answer));
            pages += 1;
            match answer["next_cursor"].as_str() {
                Some(next) => cursor = Some(next.to_owned()),
                None => break,
            }
            assert!(pages < 1000, "paging must terminate");
        }
        assert!(pages > 1);
        let expected_tasks: Vec<String> = document
            .tasks
            .iter()
            .filter(|task| {
                include_archived || task.get("status").and_then(Value::as_str) != Some("archived")
            })
            .filter_map(|task| task.get("id").and_then(Value::as_str).map(str::to_owned))
            .collect();
        assert_eq!(
            seen_tasks, expected_tasks,
            "include_archived={include_archived}"
        );
        // agent_states is a BTreeMap, so the document order is already sorted.
        let expected_actors: Vec<String> = document.agent_states.keys().cloned().collect();
        assert_eq!(seen_actors, expected_actors);
    }
}

#[test]
fn summary_keeps_real_counts_without_items() {
    let document = real_document();
    let answer = project(
        &document,
        "ctxv:17189",
        "full",
        &bounds(json!({"summary": true}), json!({})),
    );
    assert!(answer.get("board").is_none());
    assert_eq!(answer["coordination"]["tasks"], json!([]));
    assert_eq!(answer["agent_states"], json!([]));
    assert_eq!(answer["tasks_summary"]["total"], json!(633));
    assert_eq!(answer["tasks_summary"]["archived"], json!(142));
    assert_eq!(answer["truncated"], json!(false));
    assert!(size(&answer) < 64 * 1024, "{}", size(&answer));
}

#[test]
fn include_selects_real_sections() {
    let document = real_document();
    let board_only = project(
        &document,
        "ctxv:17189",
        "full",
        &bounds(json!({"include": ["board"]}), json!({})),
    );
    assert!(board_only.get("coordination").is_none());
    assert!(board_only.get("agent_states").is_none());
    assert_eq!(board_only["paging"]["task_total"], json!(633));

    let actors_only = project(
        &document,
        "ctxv:17189",
        "full",
        &bounds(json!({"include": "actors"}), json!({})),
    );
    assert!(actors_only.get("board").is_none());
    assert!(actors_only.get("coordination").is_none());
    assert!(!actor_ids(&actors_only).is_empty());
}

#[test]
fn group_caps_shrink_the_real_answer_instead_of_failing() {
    let document = real_document();
    let tiny = project(
        &document,
        "ctxv:17189",
        "full",
        &bounds(
            json!({"limit": 1000}),
            json!({"state_max_bytes": 16_384, "max_board_items": 1000}),
        ),
    );
    assert!(size(&tiny) <= 16_384, "{}", size(&tiny));
    assert_eq!(tiny["truncated"], json!(true));

    let few_items = project(
        &document,
        "ctxv:17189",
        "full",
        &bounds(json!({"limit": 1000}), json!({"max_board_items": 5})),
    );
    assert!(task_ids(&few_items).len() <= 5);
    assert!(actor_ids(&few_items).len() <= 5);
    assert_eq!(few_items["truncated"], json!(true));
}

#[test]
fn limit_zero_and_unbounded_keep_the_legacy_answer() {
    let group = group_with(json!({}));
    assert!(
        Bounds::from_request(&request(json!({"limit": 0})), &group)
            .expect("valid")
            .is_none()
    );
    assert!(
        Bounds::from_request(&request(json!({"unbounded": true})), &group)
            .expect("valid")
            .is_none()
    );
}

#[test]
fn malformed_bounds_are_rejected_with_one_clear_error() {
    let group = group_with(json!({}));
    for args in [
        json!({"cursor": "not-a-cursor"}),
        json!({"cursor": "v2.0.0"}),
        json!({"include": ["board", "history"]}),
        json!({"include": []}),
        json!({"limit": -1}),
        json!({"limit": "100"}),
    ] {
        let error = Bounds::from_request(&request(args.clone()), &group)
            .expect_err(&format!("must reject {args}"));
        assert_eq!(error.code, "invalid_args");
    }
}

#[test]
fn cursor_resumes_after_the_last_returned_item() {
    assert_eq!(next_cursor(40, 3), "v1.40.3");
    assert_eq!(parse_cursor("v1.40.3").expect("parses"), (40, 3));
}

#[test]
fn bounded_attention_is_counts_while_the_legacy_full_answer_keeps_entries() {
    let document = real_document();
    // Legacy `detail=full` returns the attention entries, which is a second
    // copy of the live tasks and a large part of the multi-megabyte answer.
    let legacy =
        super::super::context_projection::project(document.clone(), "ctxv:17189".into(), "full");
    assert!(legacy["attention"]["blocked"].is_array());

    let bounded = project(
        &document,
        "ctxv:17189",
        "full",
        &bounds(json!({"limit": 1000}), json!({})),
    );
    for key in ["blocked", "waiting_user", "pending_handoffs"] {
        assert!(
            bounded["attention"][key].is_u64(),
            "bounded attention.{key} must be a count, got {}",
            bounded["attention"][key]
        );
    }
}
