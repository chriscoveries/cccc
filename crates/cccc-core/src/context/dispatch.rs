//! Explicit automation actions over the current task store.
use cccc_contracts::Event;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::io;

use super::model::ContextDoc;
use crate::HomeLayout;

/// An idle Actor certified by the daemon's current, authoritative turn state.
/// Prices are per million tokens; a monthly subscription is not a token price.
#[derive(Debug, Clone)]
pub struct DispatchActor {
    pub actor_id: String,
    pub generation: String,
    pub usd_per_mtok: Option<f64>,
    pub strength: Option<u8>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(super) struct DispatchState {
    #[serde(default)]
    last_dispatch_ms: BTreeMap<String, i64>,
    #[serde(default)]
    idle_held: BTreeMap<String, Vec<String>>,
}

pub fn dispatch_ready_tasks(
    home: &HomeLayout,
    group_id: &str,
    rule_id: &str,
    cooldown_ms: u64,
    actors: &[DispatchActor],
    now_ms: i64,
) -> io::Result<Vec<Event>> {
    super::dispatch_transaction::run(home, group_id, |doc, state| {
        let mut actors = actors.to_vec();
        actors.retain(|actor| !actor.actor_id.is_empty() && !actor.generation.is_empty());
        actors.sort_by(|a, b| {
            price(a.usd_per_mtok)
                .unwrap_or(f64::INFINITY)
                .total_cmp(&price(b.usd_per_mtok).unwrap_or(f64::INFINITY))
                .then_with(|| strength(b.strength).cmp(&strength(a.strength)))
                .then_with(|| a.actor_id.cmp(&b.actor_id))
        });
        actors.dedup_by(|a, b| a.actor_id == b.actor_id);
        let mut tasks = doc
            .tasks
            .iter()
            .filter(|task| ready(doc, task))
            .map(|task| {
                let created = task
                    .get("created_at")
                    .and_then(Value::as_str)
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .map(|time| time.timestamp_millis())
                    .unwrap_or(i64::MAX);
                (created, text(task, "id").to_owned())
            })
            .collect::<Vec<_>>();
        tasks.sort();
        let mut used = BTreeSet::new();
        let mut events = Vec::new();
        for (_, task_id) in tasks {
            let task = doc
                .tasks
                .iter()
                .find(|task| text(task, "id") == task_id)
                .expect("listed task");
            let Some(actor) = actors.iter().find(|actor| {
                !used.contains(&actor.actor_id)
                    && !holds_open_task(doc, &actor.actor_id)
                    && state
                        .last_dispatch_ms
                        .get(&actor.actor_id)
                        .is_none_or(|last| {
                            now_ms.checked_sub(*last).is_some_and(|elapsed| {
                                elapsed >= 0 && elapsed as u64 >= cooldown_ms
                            })
                        })
                    && fits(task, actor)
            }) else {
                continue;
            };
            let title = text(task, "title").to_owned();
            let outcome = text(task, "outcome").to_owned();
            let update = json!({"op":"task.update","task_id":task_id,"assignee":actor.actor_id});
            super::task_apply::update(doc, update.as_object().expect("object"))?;
            let mut assigned = Event::new("task.assign", group_id);
            assigned.by = rule_id.to_owned();
            assigned.data = json!({"task_id":task_id,"actor_id":actor.actor_id,"generation":actor.generation,"rule_id":rule_id})
                .as_object().expect("object").clone();
            events.push(assigned);
            events.push(notice(
                group_id,
                &actor.actor_id,
                "task_dispatch",
                &format!("Assigned {task_id}: {title}"),
                &outcome,
                json!({"task_id":task_id,"rule_id":rule_id,"generation":actor.generation}),
            ));
            used.insert(actor.actor_id.clone());
            state
                .last_dispatch_ms
                .insert(actor.actor_id.clone(), now_ms);
        }
        Ok(events)
    })
}

/// A single passive notice for each changed set of open tasks held while idle.
/// This neither changes assignment nor introduces a repeat/escalation ladder.
pub fn notify_idle_held_tasks(
    home: &HomeLayout,
    group_id: &str,
    idle: &BTreeSet<String>,
) -> io::Result<Vec<Event>> {
    super::dispatch_transaction::run(home, group_id, |doc, state| {
        let mut events = Vec::new();
        for actor in idle {
            let mut tasks = doc
                .tasks
                .iter()
                .filter(|task| open(task) && text(task, "assignee") == actor)
                .map(|task| (text(task, "id").to_owned(), text(task, "title").to_owned()))
                .collect::<Vec<_>>();
            tasks.sort();
            let ids = tasks.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>();
            if ids.is_empty() {
                state.idle_held.remove(actor);
            } else if state.idle_held.get(actor) != Some(&ids) {
                let message = tasks
                    .iter()
                    .map(|(id, title)| format!("{id}: {title}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                events.push(notice(
                    group_id,
                    actor,
                    "idle_held_tasks",
                    "You hold open tasks",
                    &message,
                    json!({"task_ids":ids}),
                ));
                state.idle_held.insert(actor.clone(), ids);
            }
        }
        Ok(events)
    })
}

fn notice(
    group: &str,
    actor: &str,
    kind: &str,
    title: &str,
    message: &str,
    context: Value,
) -> Event {
    let mut event = Event::new("system.notify", group);
    event.by = "system".into();
    event.data = json!({"to":[actor],"kind":kind,"title":title,"message":message,"priority":"normal","context":context})
        .as_object().expect("object").clone();
    event
}

fn text<'a>(task: &'a Map<String, Value>, key: &str) -> &'a str {
    task.get(key).and_then(Value::as_str).unwrap_or("").trim()
}

fn open(task: &Map<String, Value>) -> bool {
    matches!(text(task, "status"), "planned" | "open" | "active")
}

fn holds_open_task(doc: &ContextDoc, actor: &str) -> bool {
    doc.tasks
        .iter()
        .any(|task| open(task) && text(task, "assignee") == actor)
}

fn ready(doc: &ContextDoc, task: &Map<String, Value>) -> bool {
    if !matches!(text(task, "status"), "planned" | "open")
        || task.get("assignee").is_some_and(|value| {
            !value.is_null() && value.as_str().is_none_or(|s| !s.trim().is_empty())
        })
        || task.get("waiting_on").is_some_and(|value| {
            !value.is_null()
                && value
                    .as_str()
                    .is_none_or(|s| !matches!(s.trim(), "" | "none"))
        })
        || !super::yaml_files::is_task_id(text(task, "id"))
    {
        return false;
    }
    if let Some(blockers) = task.get("blocked_by").filter(|v| !v.is_null()) {
        let Some(blockers) = blockers.as_array() else {
            return false;
        };
        if !blockers.iter().all(|id| {
            id.as_str().is_some_and(|id| {
                doc.tasks
                    .iter()
                    .any(|other| text(other, "id") == id && text(other, "status") == "done")
            })
        }) {
            return false;
        }
    }
    let spent = match task.get("cost_usd") {
        None | Some(Value::Null) => 0.0,
        Some(value) => match price(value.as_f64()) {
            Some(value) => value,
            None => return false,
        },
    };
    match task.get("target_budget_usd").filter(|v| !v.is_null()) {
        None => true,
        Some(value) => price(value.as_f64()).is_some_and(|target| spent < target),
    }
}

fn fits(task: &Map<String, Value>, actor: &DispatchActor) -> bool {
    match task.get("max_usd_per_mtok").filter(|v| !v.is_null()) {
        None => true,
        Some(value) => price(value.as_f64())
            .zip(price(actor.usd_per_mtok))
            .is_some_and(|(max, actual)| actual <= max),
    }
}
fn price(value: Option<f64>) -> Option<f64> {
    value.filter(|v| v.is_finite() && *v >= 0.0)
}
fn strength(value: Option<u8>) -> Option<u8> {
    value.filter(|v| (1..=5).contains(v))
}
