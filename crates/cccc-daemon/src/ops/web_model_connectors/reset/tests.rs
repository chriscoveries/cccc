use super::*;
use crate::ops::runtime_completion::{self, Completion};
use cccc_contracts::Actor;

struct Case {
    _temp: tempfile::TempDir,
    home: HomeLayout,
    group: String,
    target: Value,
    connector: String,
}

impl Case {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let home = HomeLayout::from_path(temp.path().join("home")).unwrap();
        home.initialize().unwrap();
        let groups = GroupStore::new(home.clone()).unwrap();
        let mut group = groups.create("browser reset", "").unwrap();
        let mut actor = Actor::new("browser-actor");
        actor.runtime = ActorRuntime::WebModel;
        actor.runner = RunnerKind::Headless;
        actor.enabled = false;
        actor.created_at = "2026-11-10T13:02:27.595496Z".into();
        cccc_core::actors::add(&mut group, actor).unwrap();
        groups.save(&group).unwrap();
        let target: Value = serde_json::from_str(include_str!(
            "../../../../tests/fixtures/browser-delivery-reset/submitted.json"
        ))
        .unwrap();
        integration_state::group_update(
            &groups,
            &group.group_id,
            "web_model_browser_targets",
            |v| {
                *v = json!({"browser-actor":target});
                Ok(())
            },
        )
        .unwrap();
        let connector = store::configure(&home).unwrap()["connector"]["connector_id"]
            .as_str()
            .unwrap()
            .to_owned();
        Self {
            _temp: temp,
            home,
            group: group.group_id,
            target,
            connector,
        }
    }
    fn req(&self, snapshot: Option<&str>) -> DaemonRequest {
        DaemonRequest { v:1, op:"web_model_delivery_reset".into(), args:json!({
            "by":"user","group_id":self.group,"actor_id":"browser-actor",
            "reason":"reviewed saved conversation", "apply":snapshot.is_some(), "expected_snapshot":snapshot
        }).as_object().unwrap().clone() }
    }
    fn source(&self, outcome: crate::ops::runtime_delivery::DeliveryOutcome<'_>) {
        let groups = GroupStore::new(self.home.clone()).unwrap();
        let group = groups.load(&self.group).unwrap();
        let actor = &group.actors[0];
        for id in self.target["last_delivery_event_ids"].as_array().unwrap() {
            let mut source = Event::new("system.notify", &self.group);
            source.id = id.as_str().unwrap().into();
            source.by = "system".into();
            source.data = json!({"to":[actor.id], "target_actor_id":actor.id})
                .as_object()
                .unwrap()
                .clone();
            let path = groups.ledger_path(&self.group).unwrap();
            if ledger::find_event(&path, &source.id).unwrap().is_none() {
                ledger::append(&path, &source).unwrap();
            }
            crate::ops::runtime_delivery::append_state(
                &self.home,
                &self.group,
                &actor.id,
                &actor.created_at,
                &source.id,
                "web_model_browser",
                outcome,
            )
            .unwrap();
        }
    }
    fn complete(&self) {
        self.source(crate::ops::runtime_delivery::DeliveryOutcome::Accepted);
        // Controlled mutation of the captured identity: not an inferred join
        // from unrelated timestamps or from historical submitted status.
        runtime_completion::append(
            &self.home,
            &self.group,
            "browser-actor",
            &Completion {
                turn_id: self.target["last_delivery_turn_id"]
                    .as_str()
                    .unwrap()
                    .into(),
                delivery_id: self.target["last_delivery_id"].as_str().unwrap().into(),
                event_ids: serde_json::from_value(self.target["last_delivery_event_ids"].clone())
                    .unwrap(),
                status: "done".into(),
            },
        )
        .unwrap();
    }
    fn bind(&self, session: &str) -> Value {
        let group = GroupStore::new(self.home.clone())
            .unwrap()
            .load(&self.group)
            .unwrap();
        let generation = cccc_core::actors::generation_identity(&group.actors[0]);
        let pair = store::begin_pairing(
            &self.home,
            &self.connector,
            &self.group,
            "browser-actor",
            &generation,
            false,
        )
        .unwrap();
        store::accept_pairing(
            &self.home,
            &self.connector,
            pair["code"].as_str().unwrap(),
            session,
        )
        .unwrap();
        store::confirm_pairing(
            &self.home,
            &self.connector,
            &self.group,
            "browser-actor",
            &generation,
            pair["pairing_id"].as_str().unwrap(),
            "https://chatgpt.com/c/redacted-conversation",
        )
        .unwrap()
    }
    fn preview(&self) -> Value {
        Value::Object(execute(&self.home, &self.req(None)).unwrap())
    }
    fn native_bind(&self, session: &str) {
        let call = |op: &str, args: Value| {
            super::super::execute(
                &self.home,
                &DaemonRequest {
                    v: 1,
                    op: op.into(),
                    args: args.as_object().unwrap().clone(),
                },
            )
            .unwrap()
        };
        let args = json!({"by":"user","connector_id":self.connector,"group_id":self.group,"actor_id":"browser-actor"});
        let pairing = call("web_model_pairing_begin", args.clone());
        call(
            "web_model_pairing_accept",
            json!({"by":"user","connector_id":self.connector,
            "code":pairing["code"],"session_key":session}),
        );
        let mut confirm = args;
        confirm["pairing_id"] = pairing["pairing_id"].clone();
        confirm["url"] = json!("https://chatgpt.com/c/redacted-fresh-conversation");
        assert_eq!(call("web_model_pairing_confirm", confirm)["state"], "bound");
    }
    fn target_now(&self) -> Value {
        integration_state::group_get(
            &GroupStore::new(self.home.clone()).unwrap(),
            &self.group,
            "web_model_browser_targets",
        )
        .unwrap()["browser-actor"]
            .clone()
    }
}

#[test]
fn captured_submitted_without_exact_receipt_is_unknown_and_refused() {
    let c = Case::new();
    let before = c.target_now();
    let preview = c.preview();
    assert!(preview["reset"]["completion_status"].is_null());
    assert_eq!(preview["reset"]["can_apply"], false);
    assert_eq!(
        execute(&c.home, &c.req(preview["reset"]["snapshot"].as_str()))
            .unwrap_err()
            .code,
        "reset_unsafe"
    );
    assert_eq!(c.target_now(), before);
}

#[test]
fn settled_handoff_is_preserved_and_actor_can_pair_again_without_replay() {
    let c = Case::new();
    c.complete();
    c.bind("old-session");
    let before = c.target_now();
    let preview = c.preview();
    assert_eq!(preview["reset"]["completion_status"], "done");
    let req = c.req(preview["reset"]["snapshot"].as_str());
    let result = execute(&c.home, &req).unwrap();
    assert_eq!(
        result["reset"]["action"],
        "retire_settled_conversation_authority"
    );
    let after = c.target_now();
    assert_eq!(after["last_delivery_status"], "resolved");
    for (key, value) in before.as_object().unwrap() {
        if !matches!(
            key.as_str(),
            "last_delivery_status" | "next_delivery" | "state"
        ) {
            assert_eq!(&after[key], value, "preserve saved evidence {key}");
        }
    }
    assert!(store::binding_for_session(&store::load(&c.home).unwrap()[0], "old-session").is_none());
    c.native_bind("fresh-session");
    // A lost-response retry cannot retire the fresh pairing.
    assert_eq!(execute(&c.home, &req).unwrap()["already_applied"], true);
    assert!(
        store::binding_for_session(&store::load(&c.home).unwrap()[0], "fresh-session").is_some()
    );
    let completion = runtime_completion::committed(
        &c.home,
        &c.group,
        "browser-actor",
        before["last_delivery_turn_id"].as_str().unwrap(),
        &serde_json::from_value::<Vec<String>>(before["last_delivery_event_ids"].clone()).unwrap(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(completion.status, "done");
}

#[test]
fn every_intent_authority_outcome_cut_recovers_without_touching_a_successor() {
    for cut in [
        Phase::Intent,
        Phase::AuthorityRetired,
        Phase::TargetResolved,
    ] {
        let c = Case::new();
        c.complete();
        c.bind("old-session");
        let preview = c.preview();
        let req = c.req(preview["reset"]["snapshot"].as_str());
        assert!(
            execute_with_hook(&c.home, &req, |phase| if phase == cut {
                Err(OpError::new("crash_cut", "injected crash boundary"))
            } else {
                Ok(())
            })
            .is_err()
        );
        if cut == Phase::TargetResolved {
            assert_eq!(c.target_now()["last_delivery_status"], "resolved");
        } else {
            assert_eq!(c.target_now(), c.target);
        }
        assert_eq!(execute(&c.home, &req).unwrap()["reset"]["applied"], true);
        c.bind("successor");
        assert_eq!(execute(&c.home, &req).unwrap()["already_applied"], true);
        assert!(
            store::binding_for_session(&store::load(&c.home).unwrap()[0], "successor").is_some()
        );
    }
}

#[test]
fn stale_preview_live_actor_active_turn_and_browser_work_are_refused() {
    let c = Case::new();
    c.complete();
    c.bind("old-session");
    let p = c.preview();
    let req = c.req(p["reset"]["snapshot"].as_str());
    c.bind("replacement-session");
    assert_eq!(execute(&c.home, &req).unwrap_err().code, "reset_conflict");
    let groups = GroupStore::new(c.home.clone()).unwrap();
    groups
        .mutate(&c.group, |g| {
            g.actors[0].enabled = true;
            Ok(())
        })
        .unwrap();
    let p = c.preview();
    assert_eq!(
        execute(&c.home, &c.req(p["reset"]["snapshot"].as_str()))
            .unwrap_err()
            .code,
        "reset_unsafe"
    );
    groups
        .mutate(&c.group, |g| {
            g.actors[0].enabled = false;
            Ok(())
        })
        .unwrap();
    integration_state::group_update(&groups, &c.group, "runtime_states", |v| {
        *v = json!({"browser-actor":{"status":"working","active_turn_id":"concurrent-turn"}});
        Ok(())
    })
    .unwrap();
    assert_eq!(c.preview()["reset"]["can_apply"], false);
    let _guard =
        crate::ops::actor_runtime::hold_browser_work(&c.home, &c.group, "browser-actor").unwrap();
    assert_eq!(
        execute(&c.home, &c.req(None)).unwrap_err().code,
        "actor_browser_busy"
    );
}

#[test]
fn nonuser_dots_and_changed_incarnation_cannot_reset() {
    let c = Case::new();
    c.complete();
    let p = c.preview();
    let req = c.req(p["reset"]["snapshot"].as_str());
    let mut denied = req.clone();
    denied.args.insert("by".into(), json!("browser-actor"));
    assert_eq!(
        super::super::execute(&c.home, &denied).unwrap_err().code,
        "permission_denied"
    );
    let groups = GroupStore::new(c.home.clone()).unwrap();
    groups
        .mutate(&c.group, |g| {
            g.actors[0].generation = "replacement".into();
            Ok(())
        })
        .unwrap();
    assert_eq!(execute(&c.home, &req).unwrap_err().code, "reset_conflict");
    groups
        .mutate(&c.group, |g| {
            g.actors[0].runtime = ActorRuntime::ChatgptDots;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        execute(&c.home, &c.req(None)).unwrap_err().code,
        "invalid_actor_runtime"
    );
}

#[test]
fn interrupted_reset_refuses_a_new_pairing_and_old_callback_authority_is_fenced() {
    for cut in [Phase::Intent, Phase::AuthorityRetired] {
        let c = Case::new();
        c.complete();
        let mut old = c.bind("old-session");
        old["connector_id"] = json!(c.connector);
        let groups = GroupStore::new(c.home.clone()).unwrap();
        groups
            .mutate(&c.group, |g| {
                g.actors[0].enabled = true;
                Ok(())
            })
            .unwrap();
        store::validate_binding(&c.home, &old).unwrap();
        groups
            .mutate(&c.group, |g| {
                g.actors[0].enabled = false;
                Ok(())
            })
            .unwrap();
        let preview = c.preview();
        let req = c.req(preview["reset"]["snapshot"].as_str());
        assert!(
            execute_with_hook(&c.home, &req, |phase| if phase == cut {
                Err(OpError::new("crash_cut", "interrupted"))
            } else {
                Ok(())
            })
            .is_err()
        );
        c.bind("successor");
        assert_eq!(execute(&c.home, &req).unwrap_err().code, "reset_conflict");
        assert!(
            store::binding_for_session(&store::load(&c.home).unwrap()[0], "successor").is_some()
        );
        groups
            .mutate(&c.group, |g| {
                g.actors[0].enabled = true;
                Ok(())
            })
            .unwrap();
        assert!(store::validate_binding(&c.home, &old).is_err());
        assert_eq!(c.target_now(), c.target);
    }
}

#[test]
fn outcome_recovery_does_not_retire_a_pairing_created_after_target_commit() {
    let c = Case::new();
    c.complete();
    c.bind("old-session");
    let preview = c.preview();
    let req = c.req(preview["reset"]["snapshot"].as_str());
    assert!(
        execute_with_hook(&c.home, &req, |phase| if phase == Phase::TargetResolved {
            Err(OpError::new("crash_cut", "outcome not appended"))
        } else {
            Ok(())
        })
        .is_err()
    );
    c.bind("successor");
    assert_eq!(execute(&c.home, &req).unwrap()["already_applied"], true);
    assert!(store::binding_for_session(&store::load(&c.home).unwrap()[0], "successor").is_some());
}

// Rebase only identity fields into a temporary group. Original source/turn/
// delivery correlation and timestamps come from the captured metadata packet;
// the old irreversibly redacted projection remains a separate negative fixture.
fn correlated(completed: bool) -> Case {
    let mut c = Case::new();
    let rows: Vec<Event> = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/browser-delivery-reset/correlated.json"
    ))
    .unwrap();
    let submitting = rows
        .iter()
        .find(|e| e.kind == "web_model.browser_delivery.submitting")
        .unwrap();
    c.target["last_delivery_id"] = submitting.data["delivery_id"].clone();
    c.target["last_delivery_turn_id"] = submitting.data["turn_id"].clone();
    c.target["last_delivery_event_ids"] = submitting.data["event_ids"].clone();
    let groups = GroupStore::new(c.home.clone()).unwrap();
    integration_state::group_update(&groups, &c.group, "web_model_browser_targets", |v| {
        v["browser-actor"] = c.target.clone();
        Ok(())
    })
    .unwrap();
    let path = groups.ledger_path(&c.group).unwrap();
    for mut event in rows
        .into_iter()
        .filter(|e| matches!(e.kind.as_str(), "system.notify" | "runtime.delivery"))
    {
        event.group_id = c.group.clone();
        if event.kind == "runtime.delivery" {
            event.data.insert(
                "delivery_id".into(),
                json!(crate::ops::runtime_delivery::delivery_id(
                    &c.group,
                    "browser-actor",
                    "2026-11-10T13:02:27.595496Z",
                    event.data["source_event_id"].as_str().unwrap()
                )),
            );
        }
        ledger::append(&path, &event).unwrap();
    }
    // Native append regenerates the deterministic receipt ID for the temporary
    // group, from the captured correlated tuple, not a timestamp-based join.
    if completed {
        c.complete();
    }
    c
}

fn seed_pending(c: &Case) {
    let groups = GroupStore::new(c.home.clone()).unwrap();
    let mut event = Event::new("system.notify", &c.group);
    event.id = "controlled-unrelated-pending".into();
    event.by = "system".into();
    event.data = json!({"to":["browser-actor"],"target_actor_id":"browser-actor"})
        .as_object()
        .unwrap()
        .clone();
    ledger::append(&groups.ledger_path(&c.group).unwrap(), &event).unwrap();
}

fn assert_queue_fences(c: &Case) {
    use crate::ops::runtime_delivery::{self, ClaimResult};
    let groups = GroupStore::new(c.home.clone()).unwrap();
    let group = groups.load(&c.group).unwrap();
    let actor = &group.actors[0];
    let old = c.target["last_delivery_event_ids"][0].as_str().unwrap();
    assert!(matches!(
        runtime_delivery::claim(&c.home, &group, actor, old, "web_model_browser", false).unwrap(),
        ClaimResult::Terminal(_)
    ));
    let mut fresh = Event::new("system.notify", &c.group);
    fresh.id = "controlled-unrelated-pending".into();
    fresh.by = "system".into();
    fresh.data = json!({"to":[actor.id],"target_actor_id":actor.id})
        .as_object()
        .unwrap()
        .clone();
    let path = groups.ledger_path(&c.group).unwrap();
    if ledger::find_event(&path, &fresh.id).unwrap().is_none() {
        ledger::append(&path, &fresh).unwrap();
    }
    let pending = runtime_delivery::pending_sources(&c.home, &group, actor, 20).unwrap();
    assert!(!pending.iter().any(|e| e.id == old));
    assert!(pending.iter().any(|e| e.id == fresh.id));
    assert_eq!(
        runtime_delivery::claim(
            &c.home,
            &group,
            actor,
            &fresh.id,
            "web_model_browser",
            false
        )
        .unwrap(),
        ClaimResult::Claimed
    );
    assert!(matches!(
        runtime_delivery::claim(
            &c.home,
            &group,
            actor,
            &fresh.id,
            "web_model_browser",
            false
        )
        .unwrap(),
        ClaimResult::Terminal(_)
    ));
}

#[test]
fn correlated_unknown_provider_outcome_can_retire_authority_without_replaying_sources() {
    for ambiguous in [false, true] {
        let c = correlated(false);
        seed_pending(&c);
        if ambiguous {
            c.source(crate::ops::runtime_delivery::DeliveryOutcome::Ambiguous(
                "controlled uncertainty",
            ));
        }
        let mut old = c.bind("old");
        old["connector_id"] = json!(c.connector);
        let preview = c.preview();
        assert!(preview["reset"]["completion_status"].is_null());
        assert_eq!(preview["reset"]["source_fenced"], true);
        assert_eq!(preview["reset"]["can_apply"], true);
        assert_eq!(preview["reset"]["action"], "abandon_browser_authority");
        assert!(
            !preview["reset"]["authorities"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let req = c.req(preview["reset"]["snapshot"].as_str());
        execute(&c.home, &req).unwrap();
        c.native_bind("fresh");
        assert!(store::validate_binding(&c.home, &old).is_err());
        assert_eq!(execute(&c.home, &req).unwrap()["already_applied"], true);
        assert_queue_fences(&c);
    }
}

fn automatic(c: &Case, snapshot: Option<&str>) -> DaemonRequest {
    let mut req = c.req(snapshot);
    req.args.insert("automatic".into(), json!(true));
    req
}

fn enable_failed_setup(c: &Case) {
    let groups = GroupStore::new(c.home.clone()).unwrap();
    groups
        .mutate(&c.group, |g| {
            g.running = true;
            g.actors[0].enabled = true;
            Ok(())
        })
        .unwrap();
    let group = groups.load(&c.group).unwrap();
    let pair = store::begin_pairing(
        &c.home,
        &c.connector,
        &c.group,
        "browser-actor",
        &cccc_core::actors::generation_identity(&group.actors[0]),
        true,
    )
    .unwrap();
    store::fail_pairing(
        &c.home,
        &c.connector,
        &c.group,
        "browser-actor",
        pair["pairing_id"].as_str().unwrap(),
        "pairing_timeout",
    )
    .unwrap();
}

#[test]
fn automatic_restart_is_guarded_once_and_keeps_old_and_new_source_fences() {
    let c = correlated(true);
    seed_pending(&c);
    enable_failed_setup(&c);
    let preview = execute(&c.home, &automatic(&c, None)).unwrap();
    assert_eq!(preview["reset"]["can_apply"], true);
    assert_eq!(preview["reset"]["action"], "restart_browser_actor");
    let req = automatic(&c, preview["reset"]["snapshot"].as_str());
    assert_eq!(execute(&c.home, &req).unwrap()["reset"]["applied"], true);
    assert_eq!(c.target_now(), c.target); // no automatic authority retirement
    assert_eq!(execute(&c.home, &req).unwrap()["already_applied"], true);
    c.native_bind("fresh");
    assert_queue_fences(&c);
}

#[test]
fn automatic_restart_refuses_stops_pauses_healthy_routes_and_unknown_outcomes() {
    for cut in [Phase::Intent, Phase::Restarted] {
        let c = correlated(true);
        enable_failed_setup(&c);
        let groups = GroupStore::new(c.home.clone()).unwrap();
        groups
            .mutate(&c.group, |g| {
                g.actors[0].enabled = false;
                Ok(())
            })
            .unwrap();
        assert_eq!(
            execute(&c.home, &automatic(&c, None)).unwrap()["reset"]["can_apply"],
            false
        );
        groups
            .mutate(&c.group, |g| {
                g.actors[0].enabled = true;
                g.state = cccc_contracts::GroupState::Paused;
                Ok(())
            })
            .unwrap();
        assert_eq!(
            execute(&c.home, &automatic(&c, None)).unwrap()["reset"]["can_apply"],
            false
        );
        groups
            .mutate(&c.group, |g| {
                g.state = cccc_contracts::GroupState::Active;
                Ok(())
            })
            .unwrap();
        let p = execute(&c.home, &automatic(&c, None)).unwrap();
        let req = automatic(&c, p["reset"]["snapshot"].as_str());
        assert!(
            execute_with_hook(&c.home, &req, |phase| if phase == cut {
                Err(OpError::new("crash_cut", "interrupted"))
            } else {
                Ok(())
            })
            .is_err()
        );
        assert_eq!(
            execute(&c.home, &req).unwrap_err().code,
            "reset_outcome_unknown"
        );
        c.native_bind("replacement");
        assert_eq!(
            execute(&c.home, &automatic(&c, None)).unwrap()["reset"]["can_apply"],
            false
        );
        assert_queue_fences(&c);
    }
}
