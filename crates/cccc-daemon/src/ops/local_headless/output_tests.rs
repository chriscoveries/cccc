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
            pending_turn_task: Mutex::new(None),
            turn_tasks: Mutex::new(std::collections::HashMap::new()),
            admission: Mutex::new(()),
            unresolved_admission: AtomicBool::new(false),
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
async fn captured_pi_and_claude_turn_usage_is_committed_once_and_readable() {
    use crate::ops::codex_voice_analyst::{
        CAPTURED_PI_LENGTH_STOP, CAPTURED_PI_TOOL_TURN, replay_captured_claude_usage_turn,
        replay_captured_pi_turn,
    };
    use cccc_contracts::{ActorRuntime, DaemonRequest};
    for (runtime, messages, input, output) in [
        (
            ActorRuntime::Pi,
            replay_captured_pi_turn(CAPTURED_PI_TOOL_TURN, "abcdef01"),
            1852,
            90,
        ),
        (
            ActorRuntime::Pi,
            replay_captured_pi_turn(CAPTURED_PI_LENGTH_STOP, "abcdef01"),
            429,
            1,
        ),
        (
            ActorRuntime::Claude,
            replay_captured_claude_usage_turn(),
            14,
            4698,
        ),
    ] {
        let mut f = Fixture::new(false, "");
        let thread = messages[0]["params"]["threadId"]
            .as_str()
            .expect("owned thread");
        f.session.managed = Arc::new(AnalystSession::for_usage_test(
            f._temp.path(),
            runtime,
            thread,
        ));
        let store = GroupStore::new(f.session.home.clone()).expect("store");
        let path = store
            .ledger_path(&f.session.group_id)
            .expect("group ledger");
        for message in &messages {
            if message["method"] == "turn/completed" {
                assert!(
                    cccc_core::ledger::read_all(&path)
                        .expect("ledger")
                        .iter()
                        .all(|event| event.kind != "turn.usage"),
                    "no usage record before settlement"
                );
                if runtime == ActorRuntime::Claude {
                    // No task association is authoritative: retain null, not an assigned-card guess.
                    f.session.status.lock().expect("state").task_id = None;
                }
            }
            handle_message(&f.session, message.clone());
        }
        let usage = cccc_core::ledger::read_all(&path)
            .expect("ledger")
            .into_iter()
            .filter(|event| event.kind == "turn.usage")
            .collect::<Vec<_>>();
        assert_eq!(
            usage.len(),
            1,
            "one settled turn must emit one group-ledger usage record"
        );
        assert_eq!(usage[0].data["actor_id"], f.session.actor_id);
        assert_eq!(usage[0].data["input_tokens"], input);
        assert_eq!(usage[0].data["output_tokens"], output);
        assert_eq!(usage[0].data["turn_count"], 1);
        assert!(!usage[0].data["model_id"].is_null());
        let turn = &messages.last().expect("completion")["params"]["turn"]["id"];
        if runtime == ActorRuntime::Claude {
            assert!(usage[0].data["task_id"].is_null());
        } else {
            assert_eq!(&usage[0].data["task_id"], turn);
        }
        // No delivery named a board task here, so nothing is charged to one.
        assert!(
            usage[0].data["task_ref"].is_null(),
            "a turn whose delivery named no task stays uncharged"
        );
        // A replay of the same generation and provider turn must not double-charge.
        for message in messages {
            handle_message(&f.session, message);
        }
        let request: DaemonRequest = serde_json::from_value(json!({"op":"turn_usage", "args":{
            "group_id":f.session.group_id,"actor_id":f.session.actor_id,"by":"user"}}))
        .expect("request");
        let response = crate::dispatch::dispatch(&f.session.home, &request);
        assert!(response.ok, "{response:?}");
        assert_eq!(
            response.result["usage"]
                .as_array()
                .expect("usage list")
                .len(),
            1
        );
        assert_eq!(response.result["usage"][0]["id"], usage[0].id);
    }
}

/// A card bound to the turn the runtime accepted stays that turn's, even when the
/// started event is observed only after another delivery has been admitted: the
/// binding is looked up by turn id, so nothing arriving in between can answer for
/// it. A batch that is deferred never gets a binding at all.
#[tokio::test]
async fn a_turn_keeps_its_card_even_when_it_is_observed_after_another_delivery() {
    use crate::ops::codex_voice_analyst::{CAPTURED_PI_TOOL_TURN, replay_captured_pi_turn};
    use cccc_contracts::ActorRuntime;

    let mut f = Fixture::new(false, "");
    let messages = replay_captured_pi_turn(CAPTURED_PI_TOOL_TURN, "abcdef01");
    let thread = messages[0]["params"]["threadId"]
        .as_str()
        .expect("owned thread");
    f.session.managed = Arc::new(AnalystSession::for_usage_test(
        f._temp.path(),
        ActorRuntime::Pi,
        thread,
    ));
    let synced = cccc_core::context::ContextStore::new(f.session.home.clone())
        .expect("context store")
        .sync(
            &f.session.group_id,
            &[
                json!({"op":"task.create","title":"Running turn"})
                    .as_object()
                    .cloned()
                    .expect("operation"),
                json!({"op":"task.create","title":"Deferred batch"})
                    .as_object()
                    .cloned()
                    .expect("operation"),
            ],
            None,
            "user",
            false,
        )
        .expect("create tasks");
    let running_card = synced.changes[0]["task_id"]
        .as_str()
        .expect("task id")
        .to_owned();
    let deferred_card = synced.changes[1]["task_id"]
        .as_str()
        .expect("task id")
        .to_owned();

    let turn_id = messages
        .iter()
        .find_map(|message| message.pointer("/params/turn/id").and_then(Value::as_str))
        .expect("captured turn id")
        .to_owned();

    // The runtime accepted A and named its turn, so A's delivery binds its card.
    f.session
        .bind_turn_task(&turn_id, Some(running_card.clone()));
    // Before A's started event is observed, a second delivery is admitted and its
    // own turn accepted, binding its card. It is the later binding, and it must
    // not become A's.
    f.session
        .bind_turn_task("captured-turn-b", Some(deferred_card.clone()));
    for message in &messages {
        if message["method"] == "turn/completed" {
            break;
        }
        handle_message(&f.session, message.clone());
    }
    assert_eq!(
        f.session.running_turn_task().as_deref(),
        Some(running_card.as_str()),
        "the late started event still answers with the card its own delivery named"
    );

    // B never started here, so its binding is still waiting for its own turn and
    // is untouched by A's settlement.
    assert_eq!(
        f.session
            .take_turn_task("captured-turn-b")
            .flatten()
            .as_deref(),
        Some(deferred_card.as_str()),
        "another delivery's card is not spent on this turn"
    );
    // A batch that is deferred before the runtime names a turn binds nothing, so
    // there is no card for a later turn to inherit.
    assert_eq!(
        f.session.take_turn_task("never-started"),
        None,
        "a deferred batch leaves no card behind"
    );

    for message in &messages {
        if message["method"] != "turn/completed" {
            continue;
        }
        handle_message(&f.session, message.clone());
    }

    let path = GroupStore::new(f.session.home.clone())
        .expect("store")
        .ledger_path(&f.session.group_id)
        .expect("group ledger");
    let usage = cccc_core::ledger::read_all(&path)
        .expect("ledger")
        .into_iter()
        .filter(|event| event.kind == "turn.usage")
        .collect::<Vec<_>>();
    assert_eq!(usage.len(), 1, "one settled turn, one usage record");
    assert_eq!(
        usage[0].data["task_ref"], running_card,
        "the running turn is charged to the card it started with"
    );
    assert_ne!(
        usage[0].data["task_ref"], deferred_card,
        "a batch that never became a turn does not take over an open one"
    );
    assert_eq!(
        f.session.take_turn_task(&turn_id),
        None,
        "no binding outlives the turn that carried it"
    );
    assert!(
        f.session.running_turn_task().is_none(),
        "no turn is running any more"
    );
}

/// The other order the runtime can take: it reports the turn started before it
/// answers the admission, so the card is claimed by the start event and the
/// acceptance that arrives afterwards has nothing left to bind. The turn is
/// still charged to the card its own delivery named.
#[tokio::test]
async fn a_turn_observed_before_its_acceptance_keeps_the_card_its_delivery_named() {
    use crate::ops::codex_voice_analyst::{CAPTURED_PI_TOOL_TURN, replay_captured_pi_turn};
    use cccc_contracts::ActorRuntime;

    let mut f = Fixture::new(false, "");
    let messages = replay_captured_pi_turn(CAPTURED_PI_TOOL_TURN, "abcdef01");
    let thread = messages[0]["params"]["threadId"]
        .as_str()
        .expect("owned thread");
    f.session.managed = Arc::new(AnalystSession::for_usage_test(
        f._temp.path(),
        ActorRuntime::Pi,
        thread,
    ));
    let synced = cccc_core::context::ContextStore::new(f.session.home.clone())
        .expect("context store")
        .sync(
            &f.session.group_id,
            &[json!({"op":"task.create","title":"Early observed turn"})
                .as_object()
                .cloned()
                .expect("operation")],
            None,
            "user",
            false,
        )
        .expect("create task");
    let card = synced.changes[0]["task_id"]
        .as_str()
        .expect("task id")
        .to_owned();
    let turn_id = messages
        .iter()
        .find_map(|message| message.pointer("/params/turn/id").and_then(Value::as_str))
        .expect("captured turn id")
        .to_owned();

    // The delivery named this card and its admission is still open when the
    // runtime reports the turn.
    f.session.set_pending_turn_task(Some(card.clone()));
    for message in &messages {
        if message["method"] == "turn/completed" {
            break;
        }
        handle_message(&f.session, message.clone());
    }
    assert_eq!(
        f.session.running_turn_task().as_deref(),
        Some(card.as_str()),
        "the start event took the card of the delivery it belongs to"
    );

    // Acceptance arrives afterwards and finds the card already claimed.
    assert_eq!(
        f.session.take_pending_turn_task(),
        None,
        "a claimed card is not left for the acceptance to bind again"
    );
    f.session
        .bind_turn_task(&turn_id, f.session.take_pending_turn_task());
    assert_eq!(
        f.session.running_turn_task().as_deref(),
        Some(card.as_str()),
        "binding nothing leaves the open turn's card as it was"
    );

    for message in &messages {
        if message["method"] != "turn/completed" {
            continue;
        }
        handle_message(&f.session, message.clone());
    }

    let path = GroupStore::new(f.session.home.clone())
        .expect("store")
        .ledger_path(&f.session.group_id)
        .expect("group ledger");
    let usage = cccc_core::ledger::read_all(&path)
        .expect("ledger")
        .into_iter()
        .filter(|event| event.kind == "turn.usage")
        .collect::<Vec<_>>();
    assert_eq!(usage.len(), 1, "one settled turn, one usage record");
    assert_eq!(
        usage[0].data["task_ref"],
        card.as_str(),
        "the turn is charged to the card its early start event claimed"
    );
}

/// A turn whose own delivery named no card is unattributed and stays that way: it
/// does not take the card of the delivery being admitted while it finishes.
#[tokio::test]
async fn an_unattributed_turn_never_claims_another_deliverys_card() {
    use crate::ops::codex_voice_analyst::{CAPTURED_PI_TOOL_TURN, replay_captured_pi_turn};
    use cccc_contracts::ActorRuntime;

    let mut f = Fixture::new(false, "");
    let messages = replay_captured_pi_turn(CAPTURED_PI_TOOL_TURN, "abcdef01");
    let thread = messages[0]["params"]["threadId"]
        .as_str()
        .expect("owned thread");
    f.session.managed = Arc::new(AnalystSession::for_usage_test(
        f._temp.path(),
        ActorRuntime::Pi,
        thread,
    ));
    let synced = cccc_core::context::ContextStore::new(f.session.home.clone())
        .expect("context store")
        .sync(
            &f.session.group_id,
            &[json!({"op":"task.create","title":"The next delivery"})
                .as_object()
                .cloned()
                .expect("operation")],
            None,
            "user",
            false,
        )
        .expect("create task");
    let next_card = synced.changes[0]["task_id"]
        .as_str()
        .expect("task id")
        .to_owned();
    let turn_id = messages
        .iter()
        .find_map(|message| message.pointer("/params/turn/id").and_then(Value::as_str))
        .expect("captured turn id")
        .to_owned();

    // This turn's delivery named no card, so its acceptance resolved it to
    // nothing at all rather than leaving it unresolved.
    f.session.bind_turn_task(&turn_id, None);
    // Another delivery is now being admitted, holding its own card.
    f.session.set_pending_turn_task(Some(next_card.clone()));
    for message in &messages {
        handle_message(&f.session, message.clone());
    }

    let path = GroupStore::new(f.session.home.clone())
        .expect("store")
        .ledger_path(&f.session.group_id)
        .expect("group ledger");
    let usage = cccc_core::ledger::read_all(&path)
        .expect("ledger")
        .into_iter()
        .filter(|event| event.kind == "turn.usage")
        .collect::<Vec<_>>();
    assert_eq!(usage.len(), 1, "one settled turn, one usage record");
    assert!(
        usage[0].data["task_ref"].is_null(),
        "a turn that named no card is charged to nothing, got {}",
        usage[0].data["task_ref"]
    );
    assert_eq!(
        f.session.take_pending_turn_task().as_deref(),
        Some(next_card.as_str()),
        "the delivery being admitted keeps its own card"
    );
}

/// An admission the runtime never confirmed may still start a turn, and that turn's
/// events cannot be told apart from the next delivery's. Until the session settles,
/// no start event takes a card, so the ambiguity costs attribution rather than
/// charging one delivery's turn to another's card.
#[tokio::test]
async fn an_unconfirmed_admission_keeps_the_next_start_unattributed() {
    use crate::ops::codex_voice_analyst::{CAPTURED_PI_TOOL_TURN, replay_captured_pi_turn};
    use cccc_contracts::ActorRuntime;

    let mut f = Fixture::new(false, "");
    let messages = replay_captured_pi_turn(CAPTURED_PI_TOOL_TURN, "abcdef01");
    let thread = messages[0]["params"]["threadId"]
        .as_str()
        .expect("owned thread");
    f.session.managed = Arc::new(AnalystSession::for_usage_test(
        f._temp.path(),
        ActorRuntime::Pi,
        thread,
    ));
    let synced = cccc_core::context::ContextStore::new(f.session.home.clone())
        .expect("context store")
        .sync(
            &f.session.group_id,
            &[json!({"op":"task.create","title":"The next delivery"})
                .as_object()
                .cloned()
                .expect("operation")],
            None,
            "user",
            false,
        )
        .expect("create task");
    let next_card = synced.changes[0]["task_id"]
        .as_str()
        .expect("task id")
        .to_owned();

    // The previous admission was never confirmed, and another delivery now holds
    // its own card while the session is still open.
    f.session.quarantine_unresolved_admission();
    f.session.set_pending_turn_task(Some(next_card.clone()));
    for message in &messages {
        handle_message(&f.session, message.clone());
    }

    let path = GroupStore::new(f.session.home.clone())
        .expect("store")
        .ledger_path(&f.session.group_id)
        .expect("group ledger");
    let usage = cccc_core::ledger::read_all(&path)
        .expect("ledger")
        .into_iter()
        .filter(|event| event.kind == "turn.usage")
        .collect::<Vec<_>>();
    assert_eq!(usage.len(), 1, "one settled turn, one usage record");
    assert!(
        usage[0].data["task_ref"].is_null(),
        "a turn that cannot be told apart from an unconfirmed admission is not charged, got {}",
        usage[0].data["task_ref"]
    );
    assert_eq!(
        f.session.take_pending_turn_task().as_deref(),
        Some(next_card.as_str()),
        "the delivery being admitted keeps its own card"
    );
    assert!(
        !f.session.has_unresolved_admission(),
        "a settled session is no longer holding an unconfirmed admission"
    );
}

/// What the stage found, as a test: the runtime reports its own turn token as
/// the session's task id while the delivery names a board task. The board task
/// is what gets charged, and the token never is.
#[tokio::test]
async fn a_metered_turn_is_charged_to_the_task_its_delivery_named() {
    use crate::ops::codex_voice_analyst::{CAPTURED_PI_TOOL_TURN, replay_captured_pi_turn};
    use cccc_contracts::ActorRuntime;

    let mut f = Fixture::new(false, "");
    let messages = replay_captured_pi_turn(CAPTURED_PI_TOOL_TURN, "abcdef01");
    let thread = messages[0]["params"]["threadId"]
        .as_str()
        .expect("owned thread");
    f.session.managed = Arc::new(AnalystSession::for_usage_test(
        f._temp.path(),
        ActorRuntime::Pi,
        thread,
    ));
    let synced = cccc_core::context::ContextStore::new(f.session.home.clone())
        .expect("context store")
        .sync(
            &f.session.group_id,
            &[json!({"op":"task.create","title":"Review the parser"})
                .as_object()
                .cloned()
                .expect("operation")],
            None,
            "user",
            false,
        )
        .expect("create task");
    let task_id = synced.changes[0]["task_id"]
        .as_str()
        .expect("task id")
        .to_owned();

    // The delivery named this task, and the provider reports its own token.
    let turn_id = messages
        .iter()
        .find_map(|message| message.pointer("/params/turn/id").and_then(Value::as_str))
        .expect("captured turn id")
        .to_owned();
    f.session.bind_turn_task(&turn_id, Some(task_id.clone()));
    f.session.status.lock().expect("state").task_id = Some("pi-turn-token".into());
    for message in &messages {
        handle_message(&f.session, message.clone());
    }

    let path = GroupStore::new(f.session.home.clone())
        .expect("store")
        .ledger_path(&f.session.group_id)
        .expect("group ledger");
    let usage = cccc_core::ledger::read_all(&path)
        .expect("ledger")
        .into_iter()
        .filter(|event| event.kind == "turn.usage")
        .collect::<Vec<_>>();
    assert_eq!(usage.len(), 1, "one settled turn, one usage record");
    assert_eq!(
        usage[0].data["task_ref"],
        task_id.as_str(),
        "the board task the delivery named"
    );
    assert_ne!(
        usage[0].data["task_id"],
        task_id.as_str(),
        "the provider's own task token is not the board task"
    );

    let recorded = cccc_core::context::ContextStore::new(f.session.home.clone())
        .expect("context store")
        .load(&f.session.group_id)
        .expect("load")
        .tasks
        .into_iter()
        .find(|task| task["id"] == task_id.as_str())
        .expect("task");
    // Whether the runner carries a catalog for this model is not this test's
    // subject: the turn is counted exactly once, as priced or as unpriced, and
    // never both.
    let priced = recorded["cost_turns"].as_u64().unwrap_or(0);
    let unpriced = recorded["unpriced_turns"].as_u64().unwrap_or(0);
    assert_eq!(
        priced + unpriced,
        1,
        "the metered turn is counted exactly once: priced {priced}, unpriced {unpriced}"
    );
    if priced == 0 {
        assert_eq!(
            recorded["cost_usd"].as_f64().unwrap_or(0.0),
            0.0,
            "an unpriced turn adds no dollars and is not read as free"
        );
    }
    assert!(
        f.session.take_pending_turn_task().is_none(),
        "the batch's card is spent with its turn"
    );
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
