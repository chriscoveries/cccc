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
    fn complete(&self) {
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
        if !matches!(key.as_str(), "last_delivery_status" | "next_delivery") {
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
