//! Runtime admission for opt-in task automation. Never infer idle from output.
use cccc_contracts::{Actor, ActorRuntime, Event, GroupState};
use cccc_core::context::dispatch::{self, DispatchActor};
use cccc_core::{GroupDoc, HomeLayout, actors, models::ModelRegistry};
use std::collections::{BTreeSet, HashSet};

pub(super) fn idle_generation(group: &GroupDoc, actor: &Actor) -> Option<String> {
    if !actor.enabled || !super::actor_runtime_status::resolve(group, actor).running {
        return None;
    }
    if let Some(generation) = super::local_headless::idle_generation(&group.group_id, &actor.id) {
        return Some(generation);
    }
    if actor.runtime == ActorRuntime::Hermes {
        return super::turn_probe::idle_generation(&group.group_id, &actor.id);
    }
    None
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
    dispatch::dispatch_ready_tasks(
        home,
        &group.group_id,
        rule,
        cooldown,
        &candidates,
        chrono::Utc::now().timestamp_millis(),
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
