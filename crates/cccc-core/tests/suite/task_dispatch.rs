// Task graph and actor identities come from the existing redacted capture.
// Test setup unassigns/unblocks existing tasks through the product API; it
// creates no invented workload or provider usage. Prices use the operator's
// real free and per-token catalog values; None explicitly removes metadata.
use cccc_contracts::Actor;
use cccc_core::context::{
    ContextStore,
    dispatch::{self, DispatchActor},
};
use cccc_core::{GroupStore, HomeLayout, actors, automation, ledger};
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashSet};

struct Fixture {
    _temp: tempfile::TempDir,
    home: HomeLayout,
    groups: GroupStore,
    group: String,
    ready: Vec<String>,
    actors: Vec<DispatchActor>,
    now: i64,
}
impl Fixture {
    fn new() -> Self {
        let capture: Value = serde_json::from_str(include_str!(
            "../../../cccc-mcp/tests/fixtures/captured_context.json"
        ))
        .expect("real capture");
        let temp = tempfile::tempdir().expect("temp");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let groups = GroupStore::new(home.clone()).expect("groups");
        let group = groups.create("dispatch", "").expect("group").group_id;
        let dir = groups
            .group_dir(&group)
            .expect("group path")
            .join("context/tasks");
        std::fs::create_dir_all(&dir).expect("tasks");
        let tasks = capture["tasks"].as_array().expect("tasks");
        for task in tasks {
            cccc_core::fs::write_yaml(
                &dir.join(format!("{}.yaml", task["id"].as_str().expect("id"))),
                task,
            )
            .expect("seed captured task");
        }
        let original_ready = tasks
            .iter()
            .find(|t| {
                t["status"] == "planned"
                    && t.get("assignee")
                        .and_then(Value::as_str)
                        .is_none_or(str::is_empty)
                    && t.get("waiting_on")
                        .and_then(Value::as_str)
                        .is_none_or(|v| v == "none")
                    && t.get("blocked_by")
                        .and_then(Value::as_array)
                        .is_none_or(Vec::is_empty)
            })
            .expect("captured ready task");
        let mut selected = vec![original_ready];
        for task in tasks.iter().filter(|t| t["status"] == "planned") {
            if selected.len() == 4 {
                break;
            }
            if task["id"] != original_ready["id"] {
                selected.push(task);
            }
        }
        selected.sort_by_key(|task| {
            (
                task["created_at"].as_str().unwrap_or(""),
                task["id"].as_str().unwrap_or(""),
            )
        });
        let ready = selected
            .iter()
            .map(|t| t["id"].as_str().expect("id").to_owned())
            .collect::<Vec<_>>();
        let context = ContextStore::new(home.clone()).expect("context");
        let operations=ready.iter().map(|id|json!({"op":"task.update","task_id":id,"assignee":null,"waiting_on":"none","blocked_by":[]} ).as_object().expect("op").clone()).collect::<Vec<_>>();
        context
            .sync(&group, &operations, None, "user", false)
            .expect("unassign captured tasks");
        let doc = context.load(&group).expect("context");
        let mut available = capture["agent_states"]
            .as_array()
            .expect("actor capture")
            .iter()
            .filter(|a| {
                !doc.tasks.iter().any(|t| {
                    t.get("assignee") == Some(&a["actor_id"])
                        && matches!(
                            t.get("status").and_then(Value::as_str),
                            Some("planned" | "active" | "open")
                        )
                })
            })
            .collect::<Vec<_>>();
        available.sort_by_key(|a| a["actor_id"].as_str().unwrap_or(""));
        let actors = available
            .into_iter()
            .take(4)
            .map(|a| DispatchActor {
                actor_id: a["actor_id"].as_str().expect("actor").to_owned(),
                generation: a["updated_at"]
                    .as_str()
                    .expect("captured revision token")
                    .to_owned(),
                usd_per_mtok: Some(0.07),
                strength: Some(3),
            })
            .collect::<Vec<_>>();
        assert_eq!(actors.len(), 4);
        let now = chrono::Utc::now().timestamp_millis();
        Self {
            _temp: temp,
            home,
            groups,
            group,
            ready,
            actors,
            now,
        }
    }
    fn edit(&self, op: Value) {
        ContextStore::new(self.home.clone())
            .expect("store")
            .sync(
                &self.group,
                &[op.as_object().expect("op").clone()],
                None,
                "user",
                false,
            )
            .expect("edit");
    }
    fn run(&self, actors: &[DispatchActor], at: i64) -> Vec<cccc_contracts::Event> {
        dispatch::dispatch_ready_tasks(&self.home, &self.group, "dispatch", 60_000, actors, at)
            .expect("dispatch")
    }
    fn doc(&self) -> cccc_core::context::ContextDoc {
        ContextStore::new(self.home.clone())
            .expect("context")
            .load(&self.group)
            .expect("load")
    }
}

#[test]
fn captured_tasks_dispatch_oldest_by_price_strength_and_actor_id() {
    let f = Fixture::new();
    let before = f.doc();
    let mut candidates = f.actors.clone();
    candidates[3].usd_per_mtok = Some(0.0);
    candidates[3].strength = Some(2);
    candidates[2].strength = None;
    candidates.reverse();
    let events = f.run(&candidates, f.now);
    let assignments = events
        .iter()
        .filter(|e| e.kind == "task.assign")
        .collect::<Vec<_>>();
    assert_eq!(assignments.len(), 4);
    let expected = [
        &f.actors[3].actor_id,
        &f.actors[0].actor_id,
        &f.actors[1].actor_id,
        &f.actors[2].actor_id,
    ];
    for (i, event) in assignments.iter().enumerate() {
        assert_eq!(event.data["task_id"], f.ready[i]);
        assert_eq!(event.data["actor_id"], *expected[i]);
        assert_eq!(event.by, "dispatch");
        assert!(
            event.data["generation"]
                .as_str()
                .is_some_and(|v| !v.is_empty())
        );
    }
    assert_eq!(
        events.iter().filter(|e| e.kind == "system.notify").count(),
        4
    );
    let after = f.doc();
    for id in &f.ready {
        let old = before.tasks.iter().find(|t| t["id"] == *id).expect("old");
        let new = after.tasks.iter().find(|t| t["id"] == *id).expect("new");
        for (key, value) in old {
            if !matches!(key.as_str(), "assignee" | "updated_at") {
                assert_eq!(new.get(key), Some(value), "must retain {key}");
            }
        }
    }
    assert!(
        f.run(&candidates, f.now + 60_000).is_empty(),
        "held open tasks prevent additional assignment"
    );
    let recorded =
        ledger::read_all(&f.groups.ledger_path(&f.group).expect("ledger")).expect("events");
    assert_eq!(
        recorded.iter().filter(|e| e.kind == "task.assign").count(),
        4
    );
}

#[test]
fn per_token_ceiling_never_accepts_an_unknown_or_subscription_price() {
    let f = Fixture::new();
    f.edit(json!({"op":"task.update","task_id":f.ready[0],"max_usd_per_mtok":0.0}));
    let mut candidates = f.actors[..2].to_vec();
    candidates[1].usd_per_mtok = None;
    let events = f.run(&candidates, f.now);
    assert!(
        events
            .iter()
            .filter(|e| e.kind == "task.assign")
            .all(|e| e.data["task_id"] != f.ready[0])
    );
    let doc = f.doc();
    assert!(
        doc.tasks
            .iter()
            .find(|t| t["id"] == f.ready[0])
            .expect("task")
            .get("assignee")
            .is_none_or(Value::is_null)
    );
    assert!(
        !events.is_empty(),
        "later unbounded ready tasks can still be assigned"
    );
}

#[test]
fn reached_target_and_cooldown_do_not_stop_or_reassign_work() {
    let f = Fixture::new();
    f.edit(json!({"op":"task.update","task_id":f.ready[0],"target_budget_usd":0.0}));
    let events = f.run(&f.actors[..1], f.now);
    let assigned = events
        .iter()
        .find(|e| e.kind == "task.assign")
        .expect("assignment");
    assert_ne!(assigned.data["task_id"], f.ready[0]);
    f.edit(json!({"op":"task.move","task_id":assigned.data["task_id"],"status":"done"}));
    assert!(f.run(&f.actors[..1], f.now + 59_999).is_empty());
    assert_eq!(
        f.run(&f.actors[..1], f.now + 60_000)
            .iter()
            .filter(|e| e.kind == "task.assign")
            .count(),
        1
    );
}

#[test]
fn idle_dispatch_rule_is_explicitly_enabled_on_the_existing_tick() {
    let f = Fixture::new();
    let eligible = f
        .actors
        .iter()
        .map(|a| a.actor_id.clone())
        .collect::<HashSet<_>>();
    f.groups.mutate(&f.group,|g| {for a in &f.actors {actors::add(g,Actor::new(&a.actor_id))?;}g.automation=json!({"rules":[{"id":"dispatch","scope":"group","trigger":{"kind":"interval","every_seconds":60},"action":{"kind":"dispatch_ready_task"}}]}).as_object().expect("config").clone();Ok(())}).expect("rule");
    assert!(
        automation::tick_group_with_idle_actors(&f.home, &f.group, false, &eligible, &eligible)
            .expect("tick")
            .actions
            .is_empty(),
        "omitted enabled must be off"
    );
    f.groups
        .mutate(&f.group, |g| {
            g.automation.get_mut("rules").expect("rules")[0]["enabled"] = json!(true);
            Ok(())
        })
        .expect("enable");
    automation::tick_group_with_idle_actors(&f.home, &f.group, false, &eligible, &eligible)
        .expect("baseline tick");
    let state = f
        .groups
        .state_dir(&f.group)
        .expect("state")
        .join("automation.json");
    let mut saved: Value = cccc_core::fs::read_json(&state).expect("state");
    saved["rules"]["dispatch"]["last_fired_at"] = json!(f.doc().tasks[0]["created_at"]);
    cccc_core::fs::write_json(&state, &saved).expect("due");
    let tick =
        automation::tick_group_with_idle_actors(&f.home, &f.group, false, &eligible, &eligible)
            .expect("due tick");
    assert_eq!(
        tick.actions
            .iter()
            .filter(|a| matches!(a, automation::ScheduledAction::DispatchReadyTask { .. }))
            .count(),
        1,
        "enabled idle dispatch rule must prepare its task action"
    );
    assert!(
        automation::tick_group_with_idle_actors(
            &f.home,
            &f.group,
            false,
            &eligible,
            &HashSet::new()
        )
        .expect("unknown tick")
        .actions
        .is_empty(),
        "unknown PTY state never prepares dispatch"
    );
}

#[test]
fn held_task_notice_is_single_and_does_not_mutate_assignments() {
    let f = Fixture::new();
    let _ = f.run(&f.actors[..1], f.now);
    let before = f.doc().tasks;
    let idle = BTreeSet::from([f.actors[0].actor_id.clone()]);
    let first = dispatch::notify_idle_held_tasks(&f.home, &f.group, &idle).expect("notice");
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].data["kind"], "idle_held_tasks");
    assert!(
        dispatch::notify_idle_held_tasks(&f.home, &f.group, &idle)
            .expect("repeat")
            .is_empty()
    );
    assert_eq!(f.doc().tasks, before);
}

#[test]
fn an_unmet_captured_blocker_stays_unassigned() {
    let f = Fixture::new();
    let captured = f.doc();
    let blocker = captured
        .tasks
        .iter()
        .find(|task| task["status"] == "planned" && !f.ready.iter().any(|id| task["id"] == *id))
        .expect("real planned blocker")["id"]
        .clone();
    f.edit(json!({"op":"task.update","task_id":f.ready[0],"blocked_by":[blocker]}));
    let events = f.run(&f.actors, f.now);
    assert!(
        events
            .iter()
            .filter(|e| e.kind == "task.assign")
            .all(|e| e.data["task_id"] != f.ready[0])
    );
}

#[test]
fn concurrent_passes_never_assign_a_task_or_actor_twice() {
    let f = Fixture::new();
    std::thread::scope(|scope| {
        let first = scope.spawn(|| f.run(&f.actors, f.now));
        let second = scope.spawn(|| f.run(&f.actors, f.now));
        let events = first
            .join()
            .expect("first")
            .into_iter()
            .chain(second.join().expect("second"))
            .filter(|event| event.kind == "task.assign")
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 4);
        let tasks = events
            .iter()
            .map(|e| e.data["task_id"].as_str().expect("task"))
            .collect::<BTreeSet<_>>();
        let actors = events
            .iter()
            .map(|e| e.data["actor_id"].as_str().expect("actor"))
            .collect::<BTreeSet<_>>();
        assert_eq!(tasks.len(), 4);
        assert_eq!(actors.len(), 4);
    });
}
