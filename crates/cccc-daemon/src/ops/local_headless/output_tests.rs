//! Turn outcomes through the actual managed Actor message entry point.
//! The provider transport is inert; these tests create no provider processes.

use super::*;
use crate::ops::codex_voice_analyst::AnalystSession;
use cccc_core::{GroupStore, HomeLayout};
use std::sync::{Arc, Mutex, atomic::AtomicBool};

struct Fixture {
    session: Session,
    _directory: crate::ops::local_headless::supervisor::control_fixture::ControlDirectory,
    _listener: tokio::net::UnixListener,
    _temp: tempfile::TempDir,
}

impl Fixture {
    fn new(structured: bool, private: &str) -> Self {
        let temp = tempfile::tempdir().expect("isolated output fixture");
        let config = temp.path().canonicalize().expect("canonical fixture path");
        let (listener, directory) =
            crate::ops::local_headless::supervisor::control_fixture::bind(&config);
        let home = HomeLayout::from_path(config.join("home")).expect("isolated home");
        let store = GroupStore::new(home.clone()).expect("isolated group store");
        let group = store
            .create("output outcome fixture", "")
            .expect("fixture group");
        let session = Session {
            home,
            group_id: group.group_id,
            actor_id: "fixture-actor".into(),
            managed: Arc::new(AnalystSession::for_output_test(
                &config, structured, private,
            )),
            has_terminal: AtomicBool::new(false),
            viewer: Mutex::new(None),
            status: Mutex::new(super::super::HeadlessStatus {
                status: "idle".into(),
                task_id: None,
                updated_at: String::new(),
                pid: None,
                reason: None,
            }),
            stopped: AtomicBool::new(false),
            stop_lock: Mutex::new(()),
            startup_prompt: Mutex::new(None),
            active_turn: Mutex::new(None),
            delivery_tasks: Mutex::new(std::collections::HashMap::new()),
            admission: Mutex::new(()),
        };
        Self {
            session,
            _directory: directory,
            _listener: listener,
            _temp: temp,
        }
    }

    fn start(&self, turn_id: &str) {
        handle_message(
            &self.session,
            json!({"method":"turn/started","params":{"threadId":"abcdef01","turn":{"id":turn_id}}}),
        );
    }

    fn complete(&self, turn_id: &str, status: &str, error: Value) {
        handle_message(
            &self.session,
            json!({"method":"turn/completed","params":{"threadId":"abcdef01","turn":{"id":turn_id,"status":status,"error":error}}}),
        );
    }

    fn events(&self) -> Vec<Value> {
        let path = GroupStore::new(self.session.home.clone())
            .expect("read fixture store")
            .state_dir(&self.session.group_id)
            .expect("fixture state dir")
            .join("headless/events.jsonl");
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("normalized journal event"))
            .collect()
    }

    fn terminals(&self) -> Vec<Value> {
        self.events()
            .into_iter()
            .filter(|event| {
                matches!(
                    event["type"].as_str(),
                    Some(
                        "headless.turn.completed"
                            | "headless.turn.failed"
                            | "headless.turn.cancelled"
                    )
                )
            })
            .collect()
    }
}

#[tokio::test]
async fn structured_failure_has_one_terminal_and_clears_on_the_next_turn() {
    let f = Fixture::new(true, "");
    f.start("turn-1");
    f.complete("turn-1", "failed", json!("fixture provider failure"));
    f.complete("turn-1", "failed", json!("duplicate provider notification"));
    let terminals = f.terminals();
    assert_eq!(terminals.len(), 1, "one failure has one journal terminal");
    assert_eq!(terminals[0]["type"], "headless.turn.failed");
    assert_eq!(terminals[0]["data"]["stream_id"], "abcdef01:turn-1");
    assert_eq!(terminals[0]["data"]["status"], "failed");
    assert_eq!(
        f.events()
            .iter()
            .filter(|event| event["type"] == "headless.message.completed")
            .count(),
        1,
        "message and turn are each closed once"
    );
    assert_eq!(f.session.status.lock().expect("state").status, "error");
    assert!(f.session.active_turn.lock().expect("active").is_none());
    f.start("turn-2");
    assert_eq!(f.session.status.lock().expect("state").status, "working");
    assert!(f.session.status.lock().expect("state").reason.is_none());
    f.complete("turn-2", "completed", Value::Null);
    assert_eq!(f.terminals().len(), 2);
    assert_eq!(f.terminals()[1]["type"], "headless.turn.completed");
    assert_eq!(f.session.status.lock().expect("state").status, "idle");
    assert!(f.session.status.lock().expect("state").reason.is_none());
}

#[tokio::test]
async fn structured_cancellation_closes_once_without_a_failure() {
    for status in ["cancelled", "canceled", "interrupted"] {
        let f = Fixture::new(true, "");
        f.start("turn-1");
        f.complete("turn-1", status, json!("operator interruption"));
        let terminals = f.terminals();
        assert_eq!(terminals.len(), 1, "cancellation is one terminal outcome");
        assert_eq!(terminals[0]["type"], "headless.turn.cancelled");
        assert_eq!(terminals[0]["data"]["stream_id"], "abcdef01:turn-1");
        assert_eq!(terminals[0]["data"]["status"], "cancelled");
        assert!(terminals[0]["data"]["error"].is_null());
        assert_eq!(f.session.status.lock().expect("state").status, "idle");
        assert!(f.session.status.lock().expect("state").reason.is_none());
    }
}

#[tokio::test]
async fn foreign_turn_and_thread_frames_do_not_change_the_owned_stream() {
    let f = Fixture::new(true, "");
    f.start("turn-main");
    let before = f.events();
    for message in [
        json!({"method":"turn/completed","params":{"threadId":"abcdef01","turn":{"id":"turn-old","status":"failed","error":"old failure"}}}),
        json!({"method":"item/agentMessage/delta","params":{"threadId":"abcdef01","turnId":"turn-old","delta":"old progress"}}),
        json!({"method":"turn/started","params":{"threadId":"foreign-thread","turn":{"id":"turn-sub"}}}),
        json!({"method":"turn/completed","params":{"threadId":"foreign-thread","turn":{"id":"turn-main","status":"failed","error":"sub-agent failure"}}}),
        json!({"method":"item/agentMessage/delta","params":{"threadId":"foreign-thread","turnId":"turn-main","delta":"sub-agent progress"}}),
    ] {
        handle_message(&f.session, message);
    }
    assert_eq!(
        f.events(),
        before,
        "foreign frames must not reach the journal"
    );
    assert!(
        f.session
            .active_turn
            .lock()
            .expect("active")
            .as_ref()
            .is_some_and(|turn| turn.turn_id == "turn-main")
    );
    assert_eq!(f.session.status.lock().expect("state").status, "working");
}

#[tokio::test]
async fn native_failure_and_recovery_preserve_native_completion_semantics() {
    let f = Fixture::new(false, "");
    f.start("turn-1");
    f.complete("turn-1", "failed", json!("fixture provider failure"));
    assert!(f.session.active_turn.lock().expect("active").is_none());
    assert_eq!(f.session.status.lock().expect("state").status, "error");
    assert!(
        f.session.status.lock().expect("state").reason.as_deref()
            == Some("fixture provider failure")
    );
    assert_eq!(f.terminals().len(), 1);
    assert_eq!(f.terminals()[0]["data"]["stream_id"], "abcdef01:turn-1");
    f.start("turn-2");
    assert_eq!(f.session.status.lock().expect("state").status, "working");
    assert!(f.session.status.lock().expect("state").reason.is_none());
    f.complete("turn-2", "completed", Value::Null);
    assert_eq!(f.session.status.lock().expect("state").status, "idle");
    assert!(f.session.status.lock().expect("state").reason.is_none());
    assert_eq!(
        f.terminals().len(),
        1,
        "native success still only updates status"
    );
    assert!(
        f.events()
            .iter()
            .all(|event| event["type"] != "headless.message.completed")
    );
}

#[tokio::test]
async fn progress_for_a_settled_turn_does_not_reopen_its_stream() {
    let f = Fixture::new(true, "");
    f.start("turn-1");
    f.complete("turn-1", "completed", Value::Null);
    let before = f.events();
    handle_message(
        &f.session,
        json!({"method":"item/agentMessage/delta","params":{"threadId":"abcdef01","turnId":"turn-1","delta":"late provider progress"}}),
    );
    assert_eq!(
        f.events(),
        before,
        "settled progress must not revive a stream"
    );
    assert!(f.session.active_turn.lock().expect("active").is_none());
    assert_eq!(f.session.status.lock().expect("state").status, "idle");
}

#[tokio::test]
async fn failed_outcomes_share_one_bounded_private_safe_diagnostic() {
    const PRIVATE: &str = "Synthetic-PRIVATE-Setting-7B35e99B05";
    const OPAQUE: &str = "unselected-provider-payload";
    for structured in [false, true] {
        let f = Fixture::new(structured, PRIVATE);
        f.start("turn-private");
        f.complete(
            "turn-private",
            "failed",
            json!({"message":format!("Provider rejected {PRIVATE}: {} {PRIVATE}", "界".repeat(1700)), "data":{"unselected":OPAQUE}}),
        );
        let reason = f
            .session
            .status
            .lock()
            .expect("state")
            .reason
            .clone()
            .expect("safe reason");
        assert!(
            !reason.contains(PRIVATE),
            "private setting must be redacted"
        );
        assert!(
            reason.contains("[redacted]"),
            "diagnostic retains safe redaction context"
        );
        assert_eq!(reason.chars().count(), 1600);
        assert!(
            f.terminals()[0]["data"]["error"].as_str() == Some(reason.as_str()),
            "status and journal must use the same sanitized diagnostic"
        );
        let journal = serde_json::to_string(&f.events()).expect("fixture journal JSON");
        assert!(
            !journal.contains(PRIVATE),
            "journal must not expose a private setting"
        );
        assert!(
            !journal.contains(OPAQUE),
            "journal must not expose arbitrary provider data"
        );

        f.start("turn-opaque");
        f.complete(
            "turn-opaque",
            "failed",
            json!({"opaque":{"private":PRIVATE,"data":OPAQUE}}),
        );
        let reason = f
            .session
            .status
            .lock()
            .expect("state")
            .reason
            .clone()
            .expect("generic reason");
        assert!(
            !reason.contains(PRIVATE) && !reason.contains(OPAQUE) && !reason.contains("opaque"),
            "unknown error JSON must become a generic diagnostic"
        );

        f.start("turn-status");
        f.complete("turn-status", PRIVATE, Value::Null);
        let journal = serde_json::to_string(&f.events()).expect("fixture journal JSON");
        assert!(
            !journal.contains(PRIVATE) && !journal.contains(&PRIVATE.to_ascii_lowercase()),
            "unknown provider status must not bypass redaction"
        );
        assert_eq!(f.terminals()[2]["data"]["status"], "failed");
    }
}

#[tokio::test]
async fn completed_with_an_error_is_projected_as_one_failed_outcome() {
    let f = Fixture::new(true, "");
    f.start("turn-1");
    f.complete("turn-1", "completed", json!("partial provider failure"));
    let terminals = f.terminals();
    assert_eq!(terminals.len(), 1);
    assert_eq!(terminals[0]["type"], "headless.turn.failed");
    assert_eq!(terminals[0]["data"]["status"], "failed");
    assert_eq!(f.session.status.lock().expect("state").status, "error");
}

#[tokio::test]
async fn a_captured_pi_length_stop_without_a_reply_reads_as_an_error() {
    // A real Pi turn that hit the model's length limit before writing any text.
    let f = Fixture::new(true, "");
    let captured = crate::ops::codex_voice_analyst::CAPTURED_PI_LENGTH_STOP;
    for message in crate::ops::codex_voice_analyst::replay_captured_pi_turn(captured, "abcdef01") {
        handle_message(&f.session, message);
    }
    let terminals = f.terminals();
    assert_eq!(terminals.len(), 1);
    assert_eq!(terminals[0]["type"], "headless.turn.failed");
    let error = terminals[0]["data"]["error"].as_str().unwrap_or_default();
    assert!(error.contains("length limit"), "{error}");
    let state = f.session.status.lock().expect("state");
    assert_eq!(state.status, "error");
    assert_eq!(state.reason.as_deref(), Some(error));
}

#[tokio::test]
async fn captured_pi_tool_calls_reach_the_journal_between_turn_start_and_answer() {
    // The captured session reads notes.txt. Marking that value private proves
    // that call arguments pass the session's redaction before the journal.
    let f = Fixture::new(true, "notes.txt");
    let captured = crate::ops::codex_voice_analyst::CAPTURED_PI_TOOL_TURN;
    for message in crate::ops::codex_voice_analyst::replay_captured_pi_turn(captured, "abcdef01") {
        handle_message(&f.session, message);
    }
    let events = f.events();
    let kinds = events
        .iter()
        .filter_map(|event| event["type"].as_str())
        .filter(|kind| *kind != "headless.message.delta")
        .collect::<Vec<_>>();
    assert_eq!(
        kinds,
        [
            "headless.turn.started",
            "headless.activity.started",
            "headless.activity.started",
            "headless.activity.completed",
            "headless.activity.completed",
            "headless.message.completed",
            "headless.turn.completed",
        ]
    );
    let activity = |kind: &str, tool: &str| {
        events
            .iter()
            .find(|event| event["type"] == kind && event["data"]["tool_name"] == tool)
            .map(|event| event["data"].clone())
            .expect("tool activity")
    };
    let bash = activity("headless.activity.completed", "bash");
    assert_eq!(bash["status"], "completed");
    assert_eq!(bash["kind"], "tool");
    assert_eq!(bash["raw_item_type"], "commandExecution");
    assert_eq!(bash["command"], "ls", "the program, not its arguments");
    assert_eq!(
        bash["activity_id"],
        activity("headless.activity.started", "bash")["activity_id"],
        "a call's start and finish share one trace entry"
    );
    let read = activity("headless.activity.started", "read");
    assert_eq!(read["status"], "started");
    assert_eq!(read["detail"], "[redacted]");
    let text = events
        .iter()
        .filter(|event| event["type"] == "headless.message.delta")
        .filter_map(|event| event["data"]["delta"].as_str())
        .collect::<String>();
    // Both assistant messages stream: the line before the calls, then the answer.
    assert_eq!(
        text,
        "I'll run the command and read the file.notes.txt contains the text \"fixture file\"."
    );
}

#[tokio::test]
async fn named_tool_activity_finishes_without_journaling_transient_or_foreign_updates() {
    let f = Fixture::new(true, "private-token");
    f.start("turn-1");
    let tool_event = |turn: &str, phase: Option<&str>| {
        let mut params = json!({
            "threadId":"abcdef01", "turnId":turn, "title":"bash",
            "detail":"cargo test private-token", "itemType":"commandExecution"
        });
        if let Some(phase) = phase {
            params["itemId"] = json!("call-1");
            params["phase"] = json!(phase);
        }
        json!({"method":"cccc/toolActivity", "params":params})
    };
    handle_message(&f.session, tool_event("turn-1", None));
    handle_message(&f.session, tool_event("another-turn", Some("started")));
    handle_message(&f.session, tool_event("turn-1", Some("started")));
    handle_message(&f.session, tool_event("turn-1", None));
    handle_message(&f.session, tool_event("turn-1", Some("failed")));
    let events = f.events();
    let activity = events
        .iter()
        .filter(|event| {
            event["type"]
                .as_str()
                .is_some_and(|kind| kind.starts_with("headless.activity."))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        activity.len(),
        2,
        "only the identified call's start and end are journaled"
    );
    assert_eq!(activity[0]["type"], "headless.activity.started");
    assert_eq!(activity[1]["type"], "headless.activity.completed");
    assert_eq!(
        activity[0]["data"]["activity_id"],
        activity[1]["data"]["activity_id"]
    );
    assert_eq!(activity[1]["data"]["summary"], "bash failed");
    assert_eq!(activity[1]["data"]["command"], "cargo test [redacted]");
    assert_eq!(
        f.session.status.lock().expect("state").status,
        "working",
        "a failed tool can be handled by the model; it does not end the turn"
    );
    f.complete("turn-1", "completed", Value::Null);
    assert_eq!(f.terminals().len(), 1);
}

#[test]
fn captured_usage_binds_only_the_owned_delivery_and_settles_once() {
    let captured: Value =
        serde_json::from_str(include_str!("../turn_usage/deliveries.json")).unwrap();
    let fixture = Fixture::new(false, "unused");
    let session = &fixture.session;
    let mut totals = crate::ops::turn_usage::Accumulator::new();
    for row in include_str!("../turn_usage/claude.jsonl").lines() {
        let row: Value = serde_json::from_str(row).unwrap();
        if row["type"] == "assistant" {
            totals.claude(&row);
        }
    }
    let usage = totals.summary("claude_transcript");
    let task_ref = captured["turns"][0]["board_task"]["task_id"]
        .as_str()
        .unwrap();
    session.bind_delivery_task("delivery", Some(task_ref.into()));
    session.bind_delivery_task("unconfirmed", Some("T002".into()));
    let start = json!({"method":"turn/started","params":{"turn":{"id":"runtime-turn"}}});
    handle_event(session, start.clone(), Some("delivery"));
    // A later, unrelated admission cannot rename an already-owned turn.
    session.bind_delivery_task("later", Some("T001".into()));
    handle_event(session, start, None);
    let completed = json!({"method":"turn/completed","params":{"turn":{
        "id":"runtime-turn","status":"completed","usage":usage}}});
    let mut foreign = completed.clone();
    foreign["params"]["turn"]["id"] = json!("foreign-turn");
    handle_event(session, foreign, None);
    handle_event(session, completed.clone(), None);
    handle_event(session, completed, None);
    let ledger = GroupStore::new(session.home.clone())
        .unwrap()
        .ledger_path(&session.group_id)
        .unwrap();
    let events = cccc_core::ledger::read_all(&ledger)
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "turn.usage")
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data["delivery_id"], "delivery");
    assert_eq!(events[0].data["task_ref"], "T001");
    assert_eq!(events[0].data["task_id"], "runtime-turn");
    assert!(session.take_delivery_task("delivery").is_none());
    handle_event(
        session,
        json!({"method":"turn/started","params":{"turn":{"id":"late-turn"}}}),
        Some("unconfirmed"),
    );
    handle_event(
        session,
        json!({"method":"turn/completed","params":{"turn":{
        "id":"late-turn","status":"cancelled","usage":totals.summary("claude_transcript")}}}),
        None,
    );
    let events = cccc_core::ledger::read_all(&ledger)
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "turn.usage")
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 2);
    assert_eq!(events[1].data["task_ref"], "T002");
    assert_eq!(events[1].data["delivery_id"], "unconfirmed");
    assert_eq!(events[1].data["status"], "cancelled");
    assert_eq!(events[1].data["input_tokens"], 14);
    assert!(session.take_delivery_task("unconfirmed").is_none());
    assert!(
        Fixture::new(false, "unused")
            .session
            .take_delivery_task("later")
            .is_none()
    );
    session.bind_delivery_task("conflict", Some("T001".into()));
    session.bind_delivery_task("conflict", Some("T002".into()));
    assert_eq!(session.take_delivery_task("conflict"), Some(None));
    session.bind_delivery_task("rejected", Some("T001".into()));
    session.forget_delivery_task("rejected");
    assert!(session.take_delivery_task("rejected").is_none());
}

#[test]
fn consumed_delivery_cannot_be_rebound_to_another_task() {
    let fixture = Fixture::new(false, "unused");
    let session = &fixture.session;
    session.bind_delivery_task("delivery", Some("T001".into()));
    handle_event(
        session,
        json!({"method":"turn/started","params":{"turn":{"id":"first"}}}),
        Some("delivery"),
    );
    session.bind_delivery_task("delivery", Some("T002".into()));
    assert!(
        session
            .active_turn
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .task
            .is_none()
    );
    assert!(session.take_delivery_task("delivery").is_none());
    fixture.complete("first", "completed", Value::Null);
    handle_event(
        session,
        json!({"method":"turn/started","params":{"turn":{"id":"later"}}}),
        Some("delivery"),
    );
    let active = session.active_turn.lock().unwrap();
    assert!(active.as_ref().unwrap().task.is_none());
    assert!(active.as_ref().unwrap().delegation.is_none());
}
