use cccc_contracts::{Actor, ActorRuntime, DaemonRequest, Event, GroupState, RunnerKind};
use cccc_core::{GroupDoc, GroupStore, HomeLayout, Scope, actors, ledger};
use serde_json::{Map, Value, json};
use std::time::{Duration, Instant};

use super::actor_activity::Publisher;
use super::{actor_delivery, actor_health, actor_listing, actor_runtime, runtime_restore};

fn row(home: &HomeLayout, store: &GroupStore, group_id: &str) -> Value {
    let group = store.load(group_id).expect("group");
    let request = DaemonRequest {
        v: 1,
        op: "actor_list".into(),
        args: Map::new(),
    };
    actor_listing::list(home, &group, &request).expect("actor_list")[0].clone()
}

fn events(store: &GroupStore, group_id: &str, kind: &str) -> Vec<Event> {
    ledger::read_all(&store.ledger_path(group_id).expect("ledger"))
        .expect("events")
        .into_iter()
        .filter(|event| event.kind == kind)
        .collect()
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A terminal Actor whose state the daemon cannot see (a real `sh` in a real
/// PTY, reported as `pty_running_state_unknown`): a delivery it reads and
/// never answers counts nothing, and its row has no `ignored_deliveries_48h`.
/// A real crash of its process is a down episode: one `actor.health` event to
/// `down`, one back to `degraded` when it runs again, and none after a daemon
/// restart rebuilds the window from the ledger.
#[test]
fn an_unknown_state_counts_no_ignored_delivery_and_a_crash_counts_down_once() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    let store = GroupStore::new(home.clone()).expect("store");
    let group_id = store.create("health", "").expect("group").group_id;
    store
        .mutate(&group_id, |group: &mut GroupDoc| {
            group.scopes.push(Scope {
                scope_key: "s_project".into(),
                url: temp.path().to_string_lossy().into_owned(),
                label: "project".into(),
                git_remote: String::new(),
            });
            group.active_scope_key = "s_project".into();
            group.extra.insert(
                "delivery".into(),
                json!({"ignored_delivery_after_seconds": 1}),
            );
            let mut actor = Actor::new("peer1");
            actor.runtime = ActorRuntime::Custom;
            actor.runner = RunnerKind::Pty;
            actor.submit = cccc_contracts::ActorSubmit::Newline;
            actor.command = vec![
                "sh".into(),
                "-c".into(),
                "stty -echo; IFS= read -r preamble; IFS= read -r message; sleep 60".into(),
            ];
            actors::add(group, actor)?;
            group.running = true;
            group.state = GroupState::Active;
            Ok(())
        })
        .expect("configure group");
    runtime_restore::restore_running(&home).expect("start");
    let mut publisher = Publisher::default();
    publisher.tick(&home).expect("tick");
    let first = row(&home, &store, &group_id);
    assert_eq!(first["running"], true);
    assert_eq!(
        first["effective_working_reason"],
        "pty_running_state_unknown"
    );
    assert_eq!(first["health"], "healthy");
    assert_eq!(first["down_episodes_48h"], 0);

    let mut message = Event::new("chat.message", &group_id);
    message.by = "user".into();
    message.data = json!({"to":["peer1"],"text":"status?","message_mode":"send"})
        .as_object()
        .cloned()
        .expect("message");
    ledger::append(&store.ledger_path(&group_id).expect("ledger"), &message).expect("append");
    actor_delivery::dispatch_unread(&home, &store.load(&group_id).expect("group"), "peer1");
    wait_until("the delivery", || {
        events(&store, &group_id, "runtime.delivery")
            .iter()
            .any(|delivery| {
                delivery.data["source_event_id"] == message.id.as_str()
                    && delivery.data["state"] == "accepted"
            })
    });
    // Past the one-second setting, still unanswered.
    std::thread::sleep(Duration::from_millis(2500));
    publisher.tick(&home).expect("tick");
    let unanswered = row(&home, &store, &group_id);
    assert!(
        unanswered.get("ignored_deliveries_48h").is_none(),
        "unknown state must leave the count absent: {unanswered}"
    );
    assert_eq!(unanswered["health"], "healthy");
    let ledger_path = store.ledger_path(&group_id).expect("ledger");
    let now = chrono::Utc::now().timestamp();
    assert_eq!(
        actor_health::replay(&ledger_path, now, 1, 1).get("peer1"),
        Some(&(0, 0))
    );
    assert!(events(&store, &group_id, "actor.health").is_empty());

    let pid = cccc_runtime::status(&group_id, "peer1")
        .expect("session")
        .pid
        .expect("pid");
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(pid).expect("pid")),
        nix::sys::signal::Signal::SIGKILL,
    )
    .expect("kill");
    wait_until("the exit", || {
        cccc_runtime::status(&group_id, "peer1").is_ok_and(|status| !status.running)
    });
    let exited = cccc_runtime::status(&group_id, "peer1").expect("exited session");
    actor_delivery::shutdown_actor(&group_id, "peer1");
    actor_runtime::reconcile_exited(&home, vec![exited]).expect("record the exit");
    publisher.tick(&home).expect("tick");
    publisher.tick(&home).expect("tick");
    let down = row(&home, &store, &group_id);
    assert_eq!(down["running"], false);
    assert_eq!(down["health"], "down");
    assert_eq!(down["down_episodes_48h"], 1);
    assert!(down.get("ignored_deliveries_48h").is_none());
    let recorded = events(&store, &group_id, "actor.health");
    assert_eq!(recorded.len(), 1, "one event per change, not per tick");
    assert_eq!(recorded[0].by, "system");
    assert_eq!(
        Value::Object(recorded[0].data.clone()),
        json!({"actor_id":"peer1","health":"down","previous":"healthy","down_episodes_48h":1})
    );

    runtime_restore::restore_running(&home).expect("restart");
    publisher.tick(&home).expect("tick");
    let back = row(&home, &store, &group_id);
    assert_eq!(back["running"], true);
    assert_eq!(back["health"], "degraded");
    let recorded = events(&store, &group_id, "actor.health");
    assert_eq!(recorded.len(), 2);
    assert_eq!(recorded[1].data["health"], "degraded");
    assert_eq!(recorded[1].data["previous"], "down");

    // A daemon restart rebuilds the window from the ledger's tail: the same
    // count, the same bucket, and so no new event.
    actor_health::forget(&group_id);
    let mut restarted = Publisher::default();
    restarted.tick(&home).expect("tick");
    restarted.tick(&home).expect("tick");
    assert_eq!(row(&home, &store, &group_id)["down_episodes_48h"], 1);
    assert_eq!(events(&store, &group_id, "actor.health").len(), 2);

    actor_delivery::shutdown_actor(&group_id, "peer1");
    let _ = cccc_runtime::stop(&group_id, "peer1");
}

/// A real capture (a throwaway home, real daemon, Pi with a real provider):
/// peer1 answered one message; then its process was frozen (SIGSTOP) while a
/// second was delivered, which went `claimed` and, 30 s later, `ambiguous`,
/// never `accepted`. With a one-second setting that delivery is ignored; with
/// the setting at 0 the count is off, the stamp recorded while it was on
/// included.
#[test]
fn a_delivery_a_frozen_runtime_never_confirms_is_ignored_unless_the_count_is_off() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("ledger.jsonl");
    std::fs::write(&path, include_str!("fixtures/actor_health_frozen_pi.jsonl")).expect("fixture");
    let last = ledger::tail(&path, 1)
        .expect("tail")
        .pop()
        .expect("last event");
    assert_eq!(last.kind, "runtime.delivery");
    assert_eq!(last.data["state"], "ambiguous");
    let now = chrono::DateTime::parse_from_rfc3339(&last.ts)
        .expect("timestamp")
        .timestamp()
        + 2;
    assert_eq!(
        actor_health::replay(&path, now, 1, 1).get("peer1"),
        Some(&(0, 1))
    );
    assert_eq!(
        actor_health::replay(&path, now, 1, 0).get("peer1"),
        Some(&(0, 0))
    );
}

#[test]
fn the_ignored_delivery_delay_is_a_delivery_setting() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    let store = GroupStore::new(home.clone()).expect("store");
    let group_id = store.create("settings", "").expect("group").group_id;
    assert_eq!(
        actor_health::ignored_after_seconds(&store.load(&group_id).expect("group")),
        1800
    );
    let response = crate::dispatch::dispatch(
        &home,
        &DaemonRequest {
            v: 1,
            op: "group_settings_update".into(),
            args: json!({
                "group_id":group_id,
                "by":"user",
                "patch":{"ignored_delivery_after_seconds":600}
            })
            .as_object()
            .cloned()
            .expect("request"),
        },
    );
    assert!(response.ok, "{response:?}");
    let stored = store.load(&group_id).expect("group");
    assert_eq!(
        stored.extra["delivery"],
        json!({"ignored_delivery_after_seconds":600})
    );
    assert_eq!(actor_health::ignored_after_seconds(&stored), 600);
    assert_eq!(
        response.result["settings"]["ignored_delivery_after_seconds"],
        600
    );
}
