//! Durable rename of a stopped actor. The Group is fenced while the intent is
//! present, so retry can finish actor-keyed state before publishing the new ID.

use cccc_contracts::{Actor, DaemonRequest, Event};
use cccc_core::group::ACTOR_RENAME_PENDING;
use cccc_core::permissions::ActorAction;
use cccc_core::{GroupDoc, HomeLayout, actors, ledger};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io;

use crate::dispatch::{OpError, OpResult, object, required_arg, store, string_arg};

use super::{
    actor_rename_guards, actor_rename_state, actor_runtime, actor_runtime_status, actor_switch,
    actors::{authorize, observe_actor_process_running},
};

#[derive(Serialize, Deserialize)]
struct RenameIntent {
    old: String,
    new: String,
    generation: String,
    event: Event,
}

impl RenameIntent {
    fn new(group: &GroupDoc, actor: &Actor, new: String, request: &DaemonRequest) -> Self {
        let old = actor.id.clone();
        let generation = actors::generation_identity(actor);
        let mut event = Event::new("actor.rename", &group.group_id);
        event.by = string_arg(request, "by").unwrap_or_else(|| "user".into());
        event.data.insert("old_actor_id".into(), json!(old));
        event.data.insert("new_actor_id".into(), json!(new));
        event
            .data
            .insert("actor_generation".into(), json!(generation));
        Self {
            old,
            new,
            generation,
            event,
        }
    }
}

pub(super) fn rename(home: &HomeLayout, request: &DaemonRequest) -> OpResult {
    let group_id = required_arg(request, "group_id")?;
    let old = required_arg(request, "actor_id")?;
    let new = required_arg(request, "new_actor_id")?;
    actors::validate_rename_id(&new).map_err(OpError::invalid)?;
    let store = store(home)?;
    let group = store
        .load_for_actor_rename(&group_id)
        .map_err(OpError::not_found)?;
    authorize(&group, request, ActorAction::Rename, &old)?;
    let path = store.ledger_path(&group_id).map_err(OpError::io)?;
    let pending = group.extra.get(ACTOR_RENAME_PENDING);
    if pending.is_none()
        && group.actor_aliases.get(&old) == Some(&new)
        && let Some(actor) = actors::find(&group, &new)
        && let Some(event) = ledger::inspect(&path, |events, _| {
            events
                .iter()
                .rev()
                .find(|event| rename_pair(event, &old, &new))
                .cloned()
        })
        .map_err(OpError::io)?
    {
        return renamed_response(home, &group_id, actor, &event);
    }
    let actor = actors::find(&group, &old)
        .ok_or_else(|| OpError::new("actor_not_found", "current actor id does not exist"))?;
    if actor.internal_kind.is_some() || !actor.external_session_id.is_empty() {
        return Err(OpError::invalid("rename requires a managed actor"));
    }
    let _browser_work = actor_runtime::hold_browser_work(home, &group_id, &old)?;
    let runtime_owner =
        crate::session_owner::read_scope(|| actor_runtime::delivery_owner(&group_id, &old))
            .map_err(OpError::io)?;
    if observe_actor_process_running(home, &group, actor)?
        || actor_runtime_status::resolve(home, &group, actor).running
        || runtime_owner.is_some()
    {
        return Err(OpError::new(
            "actor_running",
            "stop the actor before renaming it",
        ));
    }
    let _guards = actor_rename_guards::lock_and_check(home, &group, &old)?;
    let intent = if let Some(value) = pending {
        let intent: RenameIntent = serde_json::from_value(value.clone()).map_err(pending_error)?;
        if intent.old != old || intent.new != new {
            return Err(pending_error("retry the same actor rename"));
        }
        intent
    } else {
        actor_switch::refuse_pending(home, &group_id, &old)?;
        actors::validate_rename_target(&group, &new).map_err(OpError::invalid)?;
        let previously_used = ledger::inspect_status(&path, |events, _, generations| {
            generations.contains_key(&new)
                || events.iter().any(|event| {
                    event.kind == "actor.remove"
                        && event.data.get("actor_id").and_then(Value::as_str) == Some(new.as_str())
                })
        })
        .map_err(OpError::io)?;
        if previously_used {
            return Err(OpError::invalid(
                "new actor id already has a ledger identity",
            ));
        }
        actor_rename_state::prepare(home, &group, &old, &new).map_err(OpError::io)?;
        let intent = RenameIntent::new(&group, actor, new, request);
        let value = serde_json::to_value(&intent).map_err(OpError::invalid)?;
        store
            .mutate_for_actor_rename(&group_id, |current| {
                if current != &group {
                    return Err(io::Error::other("group changed while planning the rename"));
                }
                current.extra.insert(ACTOR_RENAME_PENDING.into(), value);
                Ok(())
            })
            .map_err(OpError::io)?;
        intent
    };

    if intent.generation != actors::generation_identity(actor)
        || intent.event.group_id != group_id
        || !rename_pair(&intent.event, &intent.old, &intent.new)
    {
        return Err(pending_error(
            "durable rename intent does not match this actor",
        ));
    }
    super::runtime_session::native_acp::rename::checkpoint(
        home,
        &group,
        &intent.old,
        &intent.new,
        &intent.event,
    )
    .map_err(pending_error)?;
    actor_rename_state::complete(home, &group_id, &intent.old, &intent.new)
        .map_err(pending_error)?;
    match ledger::find_event(&path, &intent.event.id).map_err(pending_error)? {
        None => ledger::append(&path, &intent.event).map_err(pending_error)?,
        Some(event) if event == intent.event => {}
        Some(_) => {
            return Err(pending_error(
                "rename event id has different ledger contents",
            ));
        }
    }
    let renamed = store
        .mutate_for_actor_rename(&group_id, |current| {
            if current.extra.get(ACTOR_RENAME_PENDING)
                != serde_json::to_value(&intent).ok().as_ref()
                || actors::find(current, &intent.old)
                    .is_none_or(|actor| actors::generation_identity(actor) != intent.generation)
            {
                return Err(io::Error::other("durable rename intent changed"));
            }
            let renamed = actors::rename(current, &intent.old, &intent.new)?;
            current.extra.remove(ACTOR_RENAME_PENDING);
            Ok(renamed)
        })
        .map_err(pending_error)?;
    renamed_response(home, &group_id, &renamed, &intent.event)
}

fn rename_pair(event: &Event, old: &str, new: &str) -> bool {
    event.kind == "actor.rename"
        && event.data.get("old_actor_id").and_then(Value::as_str) == Some(old)
        && event.data.get("new_actor_id").and_then(Value::as_str) == Some(new)
}

fn pending_error(error: impl std::fmt::Display) -> OpError {
    OpError::new(
        "actor_rename_pending",
        format!("retry the same actor rename: {error}"),
    )
}

fn renamed_response(home: &HomeLayout, group_id: &str, actor: &Actor, event: &Event) -> OpResult {
    object(json!({"actor": actor_runtime::actor_response(home, group_id, actor), "event": event}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handle_request;
    use cccc_core::GroupStore;
    use std::fs;

    struct Fixture {
        _temp: tempfile::TempDir,
        home: HomeLayout,
        group: GroupDoc,
        store: GroupStore,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().expect("isolated actor home");
            let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
            let store = GroupStore::new(home.clone()).expect("store");
            let group = store.create("rename fixture", "").expect("group");
            let fixture = Self {
                _temp: temp,
                home,
                group,
                store,
            };
            let added = fixture.call(
                "actor_add",
                json!({
                    "actor_id": "old-lane", "runtime": "custom", "title": "Example lane",
                    "command": ["fixture-runtime", "--resume", "fixture-session"],
                    "env": {"FIXTURE_MODE": "persist"}
                }),
            );
            assert!(added.ok, "{:?}", added.error);
            fixture
                .store
                .mutate(&fixture.group.group_id, |doc| {
                    doc.scopes.push(cccc_core::group::Scope {
                        scope_key: "fixture-scope".into(),
                        url: "file:///example/project".into(),
                        label: "Example workspace".into(),
                        git_remote: String::new(),
                    });
                    doc.actors[0].default_scope_key = "fixture-scope".into();
                    Ok(())
                })
                .expect("actor scope");
            fixture
        }

        fn request(&self, op: &str, mut args: Value) -> DaemonRequest {
            args["group_id"] = json!(self.group.group_id);
            DaemonRequest {
                v: 1,
                op: op.into(),
                args: args.as_object().expect("request arguments").clone(),
            }
        }

        fn call(&self, op: &str, args: Value) -> cccc_contracts::DaemonResponse {
            handle_request(&self.home, &self.request(op, args))
        }

        fn rename(&self, old: &str, new: &str) -> cccc_contracts::DaemonResponse {
            self.call("actor_rename", json!({"actor_id":old,"new_actor_id":new}))
        }

        fn stored(&self) -> GroupDoc {
            self.store.load(&self.group.group_id).expect("stored group")
        }
    }

    #[test]
    fn actor_rename_keeps_generation_and_reserves_old_addresses() {
        let f = Fixture::new();
        let path = f.store.ledger_path(&f.group.group_id).expect("ledger");
        let original_ledger = fs::read(&path).expect("original ledger");
        let original = f.stored().actors[0].clone();

        let result = f.rename("old-lane", "new-lane");
        assert!(result.ok, "{:?}", result.error);
        assert_eq!(result.result["event"]["kind"], "actor.rename");
        assert_eq!(result.result["event"]["data"]["old_actor_id"], "old-lane");
        let retry = f.rename("old-lane", "new-lane");
        assert!(retry.ok, "{:?}", retry.error);
        assert_eq!(retry.result["event"]["id"], result.result["event"]["id"]);

        let stored = f.stored();
        let actor = &stored.actors[0];
        assert_eq!(actor.id, "new-lane");
        assert_eq!(actor.generation, original.generation);
        assert_eq!(actor.created_at, original.created_at);
        assert_eq!(actor.env, original.env);
        assert_eq!(actor.command, original.command);
        assert_eq!(actor.default_scope_key, original.default_scope_key);
        assert_eq!(actor.title, original.title);
        assert_eq!(actors::canonical_actor_id(&stored, "old-lane"), "new-lane");
        assert!(
            !f.call(
                "actor_add",
                json!({"actor_id":"old-lane","runtime":"custom"})
            )
            .ok
        );
        assert!(
            fs::read(&path)
                .expect("current ledger")
                .starts_with(&original_ledger)
        );

        assert!(f.rename("new-lane", "final-lane").ok);
        let final_group = f.stored();
        assert_eq!(
            actors::canonical_actor_id(&final_group, "old-lane"),
            "final-lane"
        );
        assert_eq!(
            actors::canonical_actor_id(&final_group, "new-lane"),
            "final-lane"
        );
        assert!(
            ledger::inspect_status(&path, |_, _, generations| {
                generations["old-lane"] == generations["final-lane"]
            })
            .expect("indexed generations")
        );
    }

    #[test]
    fn actor_rename_recovers_a_journaled_event_without_appending_it_twice() {
        let f = Fixture::new();
        let group = f.stored();
        let request = f.request(
            "actor_rename",
            json!({"actor_id":"old-lane","new_actor_id":"new-lane"}),
        );
        let intent = RenameIntent::new(&group, &group.actors[0], "new-lane".into(), &request);
        f.store
            .mutate_for_actor_rename(&group.group_id, |current| {
                current
                    .extra
                    .insert(ACTOR_RENAME_PENDING.into(), json!(intent));
                Ok(())
            })
            .expect("persist intent");
        let path = f.store.ledger_path(&group.group_id).expect("ledger");
        ledger::append(&path, &intent.event).expect("already journaled");
        assert_eq!(
            f.store.load(&group.group_id).expect_err("fenced").kind(),
            io::ErrorKind::WouldBlock
        );
        assert!(!f.call("actor_start", json!({"actor_id":"old-lane"})).ok);
        assert!(!f.rename("old-lane", "other-lane").ok);

        let result = handle_request(&f.home, &request);
        assert!(result.ok, "{:?}", result.error);
        assert_eq!(f.stored().actors[0].id, "new-lane");
        assert_eq!(
            ledger::inspect(&path, |events, _| {
                events
                    .iter()
                    .filter(|event| event.id == intent.event.id)
                    .count()
            })
            .expect("ledger"),
            1
        );
    }

    #[test]
    fn actor_rename_refuses_invalid_ids_and_an_active_runtime() {
        let f = Fixture::new();
        for invalid in [
            "-edge",
            "edge-",
            "two--dashes",
            "bad_name",
            "Caps",
            " padded",
            "root",
        ] {
            assert!(!f.rename("old-lane", invalid).ok, "{invalid}");
        }
        f.store
            .mutate(&f.group.group_id, |doc| {
                doc.actors[0].runtime = cccc_contracts::ActorRuntime::WebModel;
                doc.running = true;
                Ok(())
            })
            .expect("running actor fixture");
        let result = f.rename("old-lane", "new-lane");
        assert_eq!(result.error.expect("running refusal").code, "actor_running");
        assert_eq!(f.stored().actors[0].id, "old-lane");
    }
}
