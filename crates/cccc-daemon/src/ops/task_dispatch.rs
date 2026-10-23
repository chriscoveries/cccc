//! Runtime admission for opt-in task automation. Never infer idle from output.
use cccc_contracts::{Actor, Event, GroupState};
use cccc_core::context::dispatch::{self, DispatchActor};
use cccc_core::{GroupDoc, HomeLayout, actors, models::ModelRegistry};
use std::collections::{BTreeSet, HashSet};

pub(super) fn idle_generation(group: &GroupDoc, actor: &Actor) -> Option<String> {
    if !actor.enabled || !super::actor_runtime_status::resolve(group, actor).running {
        return None;
    }
    super::local_headless::idle_generation(&group.group_id, &actor.id)
}

pub(super) fn idle_ids(group: &GroupDoc) -> HashSet<String> {
    actors::visible(group)
        .filter(|actor| idle_generation(group, actor).is_some())
        .map(|a| a.id.clone())
        .collect()
}

pub(super) fn run(
    home: &HomeLayout,
    group: &GroupDoc,
    rule: &str,
    cooldown: u64,
    eligible: &HashSet<String>,
) -> std::io::Result<Vec<Event>> {
    if !matches!(group.state, GroupState::Active | GroupState::Idle) {
        return Ok(Vec::new());
    }
    let registry = ModelRegistry::load_from_path(std::path::Path::new(super::MODELS_PATH));
    let candidates = actors::visible(group)
        .filter(|a| eligible.contains(&a.id))
        .filter_map(|a| {
            let generation = idle_generation(group, a)?;
            let (usd_per_mtok, strength) = registry.resolve(&a.model);
            Some(DispatchActor {
                actor_id: a.id.clone(),
                generation,
                usd_per_mtok,
                strength,
            })
        })
        .collect::<Vec<_>>();
    dispatch::dispatch_ready_tasks_checked(
        home,
        &group.group_id,
        rule,
        cooldown,
        &candidates,
        chrono::Utc::now().timestamp_millis(),
        |candidate| {
            super::local_headless::idle_generation(&group.group_id, &candidate.actor_id).as_deref()
                == Some(candidate.generation.as_str())
        },
    )
}

pub(super) fn held(
    home: &HomeLayout,
    group: &GroupDoc,
    eligible: &HashSet<String>,
) -> std::io::Result<Vec<Event>> {
    if !matches!(group.state, GroupState::Active | GroupState::Idle)
        || group
            .extra
            .get("delivery")
            .and_then(|delivery| delivery.get("notify_idle_held_tasks"))
            .and_then(serde_json::Value::as_bool)
            != Some(true)
    {
        return Ok(Vec::new());
    }
    let idle = actors::visible(group)
        .filter(|a| eligible.contains(&a.id) && idle_generation(group, a).is_some())
        .map(|a| a.id.clone())
        .collect::<BTreeSet<_>>();
    dispatch::notify_idle_held_tasks(home, &group.group_id, &idle)
}

/// Computed per-Actor authority, not a claim inferred from terminal silence.
pub(super) fn actor_status(group: &GroupDoc) -> Vec<serde_json::Value> {
    actors::visible(group).map(|actor| {
        let generation = idle_generation(group, actor);
        let reason = if !actor.enabled { Some("actor_disabled") }
            else if !matches!(group.state, GroupState::Active | GroupState::Idle) { Some("group_not_running") }
            else if super::local_headless::registered_running(&group.group_id, &actor.id).is_none() {
                Some("no_managed_turn_authority")
            } else if !super::local_headless::supports_task_dispatch(&group.group_id, &actor.id) {
                Some("no_delivery_turn_authority")
            } else if generation.is_none() { Some("not_idle_or_not_running") }
            else { None };
        serde_json::json!({"actor_id":actor.id,"dispatchable":reason.is_none(),"reason":reason,"generation":generation})
    }).collect()
}

pub(super) fn annotate_tasks(
    home: &HomeLayout,
    group: &GroupDoc,
    doc: &mut cccc_core::context::ContextDoc,
) -> std::io::Result<()> {
    use serde_json::{Value, json};
    let cooldown = group
        .automation
        .get("rules")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|rule| {
            rule.get("enabled").and_then(Value::as_bool) == Some(true)
                && rule
                    .get("action")
                    .and_then(|a| a.get("kind"))
                    .and_then(Value::as_str)
                    == Some("dispatch_ready_task")
        })
        .map(|rule| {
            rule.get("action")
                .and_then(|a| a.get("dispatch_cooldown_ms"))
                .and_then(Value::as_u64)
                .unwrap_or(60_000)
        })
        .min();
    let disabled_reason = if cooldown.is_none() {
        Some("dispatch_disabled")
    } else if !matches!(group.state, GroupState::Active | GroupState::Idle) {
        Some("group_not_running")
    } else {
        None
    };
    if let Some(reason) = disabled_reason {
        // Opting out must not make ordinary task reads depend on a dispatch
        // state file, catalog, or running provider.
        for task in &mut doc.tasks {
            task.insert("dispatch_eligible".into(), json!(false));
            task.insert("dispatch_block_reason".into(), json!(reason));
        }
        return Ok(());
    }
    let registry = ModelRegistry::load_from_path(std::path::Path::new(super::MODELS_PATH));
    let candidates = actors::visible(group)
        .filter_map(|actor| {
            if !matches!(group.state, GroupState::Active | GroupState::Idle) {
                return None;
            }
            let generation = idle_generation(group, actor)?;
            let (usd_per_mtok, strength) = registry.resolve(&actor.model);
            Some(DispatchActor {
                actor_id: actor.id.clone(),
                generation,
                usd_per_mtok,
                strength,
            })
        })
        .collect::<Vec<_>>();
    let reasons = dispatch::task_block_reasons(
        home,
        &group.group_id,
        doc,
        &candidates,
        cooldown.unwrap_or(60_000),
        chrono::Utc::now().timestamp_millis(),
    )?;
    for task in &mut doc.tasks {
        let id = task.get("id").and_then(Value::as_str).unwrap_or("");
        let reason = if cooldown.is_none() {
            Some("dispatch_disabled")
        } else {
            reasons.get(id).copied().flatten()
        };
        task.insert("dispatch_eligible".into(), json!(reason.is_none()));
        task.insert("dispatch_block_reason".into(), json!(reason));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cccc_core::{GroupStore, context::ContextStore};
    use serde_json::Value;

    #[test]
    fn task_read_explains_missing_authority_without_changing_captured_tasks() {
        let captured: Value = serde_json::from_str(include_str!(
            "../../../cccc-mcp/tests/fixtures/captured_context.json"
        ))
        .expect("captured context");
        let temp = tempfile::tempdir().expect("temporary home");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = GroupStore::new(home.clone()).expect("groups");
        let mut group = store.create("dispatch", "").expect("group");
        group.state = GroupState::Active;
        let actor = Actor::new(
            captured["agent_states"][0]["actor_id"]
                .as_str()
                .expect("Actor"),
        );
        group.actors.push(actor);
        let statuses = actor_status(&group);
        assert_eq!(statuses[0]["dispatchable"], false);
        assert_eq!(statuses[0]["reason"], "no_managed_turn_authority");
        let dir = store
            .group_dir(&group.group_id)
            .expect("group path")
            .join("context/tasks");
        std::fs::create_dir_all(&dir).expect("tasks");
        let task = &captured["tasks"][0];
        cccc_core::fs::write_yaml(
            &dir.join(format!("{}.yaml", task["id"].as_str().expect("id"))),
            task,
        )
        .expect("captured task");
        let contexts = ContextStore::new(home.clone()).expect("contexts");
        let stored = contexts.load(&group.group_id).expect("original");
        let mut view = stored.clone();
        let state_dir = store.state_dir(&group.group_id).expect("state");
        std::fs::create_dir_all(&state_dir).expect("state directory");
        std::fs::write(
            state_dir.join("task-dispatch.json"),
            b"invalid dispatch state",
        )
        .expect("unusable opted-out state");
        annotate_tasks(&home, &group, &mut view).expect("read annotation");
        assert_eq!(view.tasks[0]["dispatch_eligible"], false);
        assert_eq!(view.tasks[0]["dispatch_block_reason"], "dispatch_disabled");
        assert_eq!(contexts.load(&group.group_id).expect("stored"), stored);
    }
}
