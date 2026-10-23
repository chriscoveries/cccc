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
        };
        Self {
            session,
            _directory: directory,
            _listener: listener,
            _temp: temp,
        }
    }

    fn pi() -> Self {
        let mut fixture = Self::new(false, "");
        fixture.session.managed = Arc::new(AnalystSession::for_usage_test(
            fixture._temp.path(),
            "abcdef01",
        ));
        fixture
    }

    fn usage(&self) -> Vec<cccc_contracts::Event> {
        let store = GroupStore::new(self.session.home.clone()).expect("fixture store");
        let path = store
            .ledger_path(&self.session.group_id)
            .expect("fixture ledger");
        cccc_core::ledger::read_all(&path)
            .expect("read fixture ledger")
            .into_iter()
            .filter(|event| event.kind == "turn.usage")
            .collect()
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

#[tokio::test]
async fn captured_pi_usage_is_committed_at_settlement_and_readable_in_the_ledger() {
    use crate::ops::codex_voice_analyst::{CAPTURED_PI_TOOL_TURN, replay_captured_pi_turn};
    let f = Fixture::pi();
    let messages = replay_captured_pi_turn(CAPTURED_PI_TOOL_TURN, "abcdef01");
    for message in &messages {
        assert!(
            f.usage().is_empty(),
            "no usage is committed before settlement"
        );
        handle_message(&f.session, message.clone());
    }
    let usage = f.usage();
    assert_eq!(
        usage.len(),
        1,
        "one settled owned turn has one usage record"
    );
    let event = &usage[0];
    let completion = &messages.last().expect("captured completion")["params"]["turn"];
    assert_eq!(event.by, f.session.actor_id);
    assert_eq!(event.data["actor_id"], f.session.actor_id);
    assert_eq!(event.data["turn_id"], completion["id"]);
    assert_eq!(event.data["generation"], f.session.managed.generation());
    assert_eq!(event.data["status"], "completed");
    assert_eq!(
        event.data.len(),
        13,
        "only selected usage and ownership fields"
    );
    for field in [
        "source",
        "model_id",
        "started_at",
        "ended_at",
        "input_tokens",
        "output_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
        "usage_samples",
    ] {
        assert_eq!(event.data[field], completion["usage"][field], "{field}");
    }
    assert_eq!(event.data["input_tokens"], 1852);
    assert_eq!(event.data["output_tokens"], 90);
    assert_eq!(event.data["cache_read_tokens"], 1536);
    assert_eq!(event.data["cache_write_tokens"], 0);

    // Both a repeated completion and a full notification replay use the same
    // captured turn identity; neither can create a second ledger record.
    handle_message(&f.session, messages.last().expect("completion").clone());
    for message in messages {
        handle_message(&f.session, message);
    }
    assert_eq!(f.usage().len(), 1);
    let request: cccc_contracts::DaemonRequest = serde_json::from_value(json!({
        "op":"ledger_tail", "args":{"group_id":f.session.group_id,"kind":"all","by":"user"}
    }))
    .expect("existing ledger request");
    let response = crate::dispatch::dispatch(&f.session.home, &request);
    assert!(response.ok, "{response:?}");
    let rows = response.result["events"]
        .as_array()
        .expect("existing ledger events");
    let rows = rows
        .iter()
        .filter(|row| row["kind"] == "turn.usage")
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], event.id);
    assert_eq!(
        rows[0]["data"],
        serde_json::to_value(&event.data).expect("usage JSON")
    );
}

#[tokio::test]
async fn captured_pi_usage_rejects_foreign_thread_turn_and_runtime() {
    use crate::ops::codex_voice_analyst::{CAPTURED_PI_TOOL_TURN, replay_captured_pi_turn};
    let f = Fixture::pi();
    let owned = replay_captured_pi_turn(CAPTURED_PI_TOOL_TURN, "abcdef01");
    let other_turn = replay_captured_pi_turn(CAPTURED_PI_TOOL_TURN, "abcdef01");
    let other_thread = replay_captured_pi_turn(CAPTURED_PI_TOOL_TURN, "other-thread");
    for message in other_thread {
        handle_message(&f.session, message);
    }
    assert!(f.usage().is_empty());
    assert!(f.session.active_turn.lock().expect("active turn").is_none());
    for message in &owned[..owned.len() - 1] {
        handle_message(&f.session, message.clone());
    }
    handle_message(
        &f.session,
        other_turn.last().expect("other completion").clone(),
    );
    assert!(
        f.usage().is_empty(),
        "a different Pi turn cannot settle the owner"
    );
    assert!(f.session.active_turn.lock().expect("active turn").is_some());
    handle_message(&f.session, owned.last().expect("owned completion").clone());
    assert_eq!(f.usage().len(), 1);

    let other_runtime = Fixture::new(true, "");
    for message in owned {
        handle_message(&other_runtime.session, message);
    }
    assert!(
        other_runtime.usage().is_empty(),
        "only the Pi adapter owns Pi usage"
    );
}

#[tokio::test]
async fn captured_pi_failed_and_cancelled_usage_survives_settlement() {
    use crate::ops::codex_voice_analyst::{
        CAPTURED_PI_LENGTH_STOP, CAPTURED_PI_TOOL_TURN, replay_captured_pi_records,
        replay_captured_pi_turn,
    };
    let records = CAPTURED_PI_TOOL_TURN
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("captured Pi record"))
        .filter(|record| record["type"] != "response")
        .collect::<Vec<_>>();
    // Cancellation uses the owned Turn's flag with the original reported usage.
    for (messages, status, input, output, cache_read) in [
        (
            replay_captured_pi_turn(CAPTURED_PI_LENGTH_STOP, "abcdef01"),
            "failed",
            429,
            1,
            1280,
        ),
        (
            replay_captured_pi_records(&records, "abcdef01", true),
            "cancelled",
            1852,
            90,
            1536,
        ),
    ] {
        let f = Fixture::pi();
        for message in &messages {
            handle_message(&f.session, message.clone());
        }
        let usage = f.usage();
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].data["status"], status);
        assert_eq!(usage[0].data["source"], "pi_rpc");
        assert_eq!(usage[0].data["input_tokens"], input);
        assert_eq!(usage[0].data["output_tokens"], output);
        assert_eq!(usage[0].data["cache_read_tokens"], cache_read);
        handle_message(&f.session, messages.last().expect("completion").clone());
        assert_eq!(f.usage().len(), 1);
    }
}
