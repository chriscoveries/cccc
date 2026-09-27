//! RS-3 through the operator surface: a scheduled rule is automation, so it faces the park like
//! message delivery does, and only a real operator clears it.

use super::{actor_listing, actor_restart_backoff, actor_runtime, actors};
use crate::dispatch::OpError;
use cccc_contracts::{Actor, ActorRuntime, DaemonRequest, RunnerKind};
use cccc_core::{GroupStore, HomeLayout};
use serde_json::{Map, Value, json};

fn fixture() -> (tempfile::TempDir, HomeLayout, String) {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    let group = GroupStore::new(home.clone())
        .expect("store")
        .create("parked actor", "")
        .expect("group");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let scope = cccc_core::group::Scope {
        scope_key: "fixture".into(),
        url: workspace.to_string_lossy().into_owned(),
        label: String::new(),
        git_remote: String::new(),
    };
    GroupStore::new(home.clone())
        .expect("store")
        .mutate(&group.group_id, |doc| {
            let mut actor = Actor::new("peer1");
            actor.runtime = ActorRuntime::Custom;
            actor.runner = RunnerKind::Pty;
            actor.command = vec!["sh".into(), "-c".into(), "sleep 30".into()];
            actor.default_scope_key = scope.scope_key.clone();
            doc.actors.push(actor);
            doc.active_scope_key = scope.scope_key.clone();
            doc.scopes.push(scope);
            doc.running = true;
            Ok(())
        })
        .expect("add actor");
    (temp, home, group.group_id)
}

fn park(home: &HomeLayout, group_id: &str) {
    for _ in 0..actor_restart_backoff::MAX_FAST_FAILURES {
        actor_restart_backoff::record_fast_failure(home, group_id, "peer1", Some(1), "boom")
            .expect("fast failure");
    }
    assert!(
        actor_restart_backoff::load(home, group_id, "peer1")
            .expect("state")
            .parked()
    );
}

fn request(op: &str, group_id: &str, by: &str) -> DaemonRequest {
    DaemonRequest {
        v: 1,
        op: op.into(),
        args: json!({"group_id": group_id, "actor_id": "peer1", "by": by})
            .as_object()
            .cloned()
            .unwrap_or_default(),
    }
}

fn run(home: &HomeLayout, request: &DaemonRequest) -> Result<Map<String, Value>, OpError> {
    let operation = actors::resolve_operation(request).expect("operation");
    operation.execute(home, request)
}

#[test]
fn an_automation_start_on_a_parked_actor_is_refused_and_keeps_the_park() {
    let (_temp, home, group_id) = fixture();
    park(&home, &group_id);

    let refused = run(&home, &request("actor_start", &group_id, "system"))
        .expect_err("a scheduled start must not clear a park");
    assert_eq!(refused.code, "actor_parked");
    assert!(
        refused.message.contains("parked"),
        "the refusal names the park: {}",
        refused.message
    );
    assert!(
        actor_runtime::status(&group_id, "peer1").is_none(),
        "no runtime was started behind the operator's back"
    );
    let state = actor_restart_backoff::load(&home, &group_id, "peer1").expect("state");
    assert!(state.parked(), "the refusal keeps the park");
    assert_eq!(
        state.consecutive_fast_failures,
        actor_restart_backoff::MAX_FAST_FAILURES
    );
}

#[test]
fn an_operator_start_clears_the_park_and_launches() {
    let (_temp, home, group_id) = fixture();
    park(&home, &group_id);

    struct Cleanup(String, String);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = cccc_runtime::stop(&self.0, &self.1);
        }
    }
    let _cleanup = Cleanup(group_id.clone(), "peer1".into());

    run(&home, &request("actor_start", &group_id, "user")).expect("operator start");
    assert!(
        !actor_restart_backoff::load(&home, &group_id, "peer1")
            .expect("state")
            .parked(),
        "an operator start opens a new epoch"
    );
    assert!(
        actor_runtime::status(&group_id, "peer1").is_some_and(|status| status.running),
        "the operator's start really launched the runtime"
    );
}

#[test]
fn an_automation_start_during_the_backoff_window_is_refused_too() {
    let (_temp, home, group_id) = fixture();
    // Two failures: recorded, and the second one starts the hold.
    for _ in 0..2 {
        actor_restart_backoff::record_fast_failure(&home, &group_id, "peer1", Some(1), "boom")
            .expect("fast failure");
    }
    let refused = run(&home, &request("actor_start", &group_id, "system"))
        .expect_err("a scheduled start inside the backoff window is refused");
    assert_eq!(refused.code, "actor_restart_backoff");
    assert!(
        actor_runtime::status(&group_id, "peer1").is_none(),
        "the refusal defers the launch rather than queueing it"
    );
    assert!(
        !actor_restart_backoff::load(&home, &group_id, "peer1")
            .expect("state")
            .parked(),
        "a deferred start is not a park"
    );
}

#[test]
fn an_automated_group_start_skips_a_parked_actor() {
    let (_temp, home, group_id) = fixture();
    park(&home, &group_id);
    let group = GroupStore::new(home.clone())
        .expect("store")
        .load(&group_id)
        .expect("group");

    let started = actor_runtime::start_group(&home, &group, true).expect("group start");
    assert!(
        started.is_empty(),
        "a parked actor is not relaunched by a rule"
    );
    assert!(actor_runtime::status(&group_id, "peer1").is_none());
    assert!(
        actor_restart_backoff::load(&home, &group_id, "peer1")
            .expect("state")
            .parked()
    );
}

#[test]
fn the_actor_listing_exposes_the_park_and_its_reason() {
    let (_temp, home, group_id) = fixture();
    park(&home, &group_id);
    let group = GroupStore::new(home.clone())
        .expect("store")
        .load(&group_id)
        .expect("group");

    let listed = actor_listing::list(&home, &group, &request("actor_list", &group_id, "user"))
        .expect("actor list");
    let actor = listed
        .first()
        .expect("one actor")
        .as_object()
        .expect("actor object");
    assert_eq!(actor["parked"], json!(true));
    assert_eq!(
        actor["parked_reason"],
        json!(format!(
            "consecutive_fast_failures={}",
            actor_restart_backoff::MAX_FAST_FAILURES
        ))
    );
    assert_eq!(
        actor["consecutive_fast_failures"],
        json!(actor_restart_backoff::MAX_FAST_FAILURES)
    );
    assert_eq!(actor["restart_next_at"], Value::Null);

    // An actor with no history is not parked, and says so rather than omitting the field.
    let healthy = actor_restart_backoff::actor_fields(&home, &group_id, "peer2");
    assert_eq!(healthy["parked"], json!(false));
    assert_eq!(healthy["parked_reason"], Value::Null);
    assert_eq!(healthy["restart_next_at"], Value::Null);
}
