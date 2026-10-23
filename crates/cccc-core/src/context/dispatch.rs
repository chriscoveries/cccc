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
    last_dispatch_task: BTreeMap<String, String>,
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
    dispatch_ready_tasks_checked(home, group_id, rule_id, cooldown_ms, actors, now_ms, |_| {
        true
    })
}

/// Daemon admission must be a fresh, nonblocking same-generation idle check.
/// It is consulted under the task lock and again at the outbox commit; it must
/// never wait for a turn-completion lock that might itself need this context.
#[allow(clippy::too_many_arguments)]
pub fn dispatch_ready_tasks_checked(
    home: &HomeLayout,
    group_id: &str,
    rule_id: &str,
    cooldown_ms: u64,
    actors: &[DispatchActor],
    now_ms: i64,
    idle: impl Fn(&DispatchActor) -> bool,
) -> io::Result<Vec<Event>> {
    let selected = std::cell::RefCell::new(Vec::<DispatchActor>::new());
    super::dispatch_transaction::run_guarded(
        home,
        group_id,
        |doc, state| {
            let mut actors = actors.to_vec();
            actors.retain(|actor| {
                !actor.actor_id.is_empty()
                    && !actor.generation.is_empty()
                    && !holds_open_task(doc, &actor.actor_id)
            });
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
                        && cooldown_ready(doc, state, &actor.actor_id, now_ms, cooldown_ms)
                        && fits(task, actor)
                }) else {
                    continue;
                };
                if !idle(actor) {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "dispatch Actor is no longer idle",
                    ));
                }
                selected.borrow_mut().push(actor.clone());
                let title = text(task, "title").to_owned();
                let outcome = text(task, "outcome").to_owned();
                let created_at = task.get("created_at").cloned();
                let cooldown_bypassed = manually_unassigned(doc, state, &actor.actor_id);
                let update =
                    json!({"op":"task.update","task_id":task_id,"assignee":actor.actor_id});
                super::task_apply::update(doc, update.as_object().expect("object"))?;
                let mut assigned = Event::new("task.assign", group_id);
                assigned.by = rule_id.to_owned();
                assigned.data = json!({"task_id":task_id,"actor_id":actor.actor_id,"generation":actor.generation,"rule_id":rule_id,
                "sort_key":{"usd_per_mtok":price(actor.usd_per_mtok),"strength":strength(actor.strength),"actor_id":actor.actor_id},
                "task_created_at":created_at,"dispatch_cooldown_ms":cooldown_ms,
                "cooldown_bypassed":cooldown_bypassed})
                .as_object().expect("object").clone();
                events.push(assigned);
                let mut push = notice(
                    group_id,
                    &actor.actor_id,
                    "task_dispatch",
                    &format!("Assigned {task_id}: {title}"),
                    &outcome,
                    json!({"task_id":task_id,"rule_id":rule_id,"generation":actor.generation}),
                );
                // The delivery's explicit ref is the only board-task authority for
                // the managed turn it starts. Runtime turn tokens stay separate.
                push.data.insert(
                    "refs".into(),
                    json!([{"kind":"task_ref","task_id":task_id}]),
                );
                events.push(push);
                used.insert(actor.actor_id.clone());
                state
                    .last_dispatch_ms
                    .insert(actor.actor_id.clone(), now_ms);
                state
                    .last_dispatch_task
                    .insert(actor.actor_id.clone(), task_id);
            }
            Ok(events)
        },
        || {
            if selected.borrow().iter().all(&idle) {
                Ok(())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "dispatch Actor changed before outbox commit",
                ))
            }
        },
    )
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
    ready_reason(doc, task).is_none()
}

fn ready_reason(doc: &ContextDoc, task: &Map<String, Value>) -> Option<&'static str> {
    if !matches!(text(task, "status"), "planned" | "open") {
        return Some("status_not_ready");
    }
    if task
        .get("assignee")
        .is_some_and(|v| !v.is_null() && v.as_str().is_none_or(|s| !s.trim().is_empty()))
    {
        return Some("assigned");
    }
    if task.get("waiting_on").is_some_and(|v| {
        !v.is_null() && v.as_str().is_none_or(|s| !matches!(s.trim(), "" | "none"))
    }) {
        return Some("waiting_on");
    }
    if !super::yaml_files::is_task_id(text(task, "id")) {
        return Some("invalid_task_id");
    }
    if let Some(blockers) = task.get("blocked_by").filter(|v| !v.is_null()) {
        let Some(blockers) = blockers.as_array() else {
            return Some("blocked_by");
        };
        if !blockers.iter().all(|id| {
            id.as_str().is_some_and(|id| {
                doc.tasks
                    .iter()
                    .any(|other| text(other, "id") == id && text(other, "status") == "done")
            })
        }) {
            return Some("blocked_by");
        }
    }
    let spent = match task.get("cost_usd") {
        None | Some(Value::Null) => 0.0,
        Some(value) => match price(value.as_f64()) {
            Some(v) => v,
            None => return Some("invalid_recorded_cost"),
        },
    };
    match task.get("target_budget_usd").filter(|v| !v.is_null()) {
        None => None,
        Some(value) => match price(value.as_f64()) {
            None => Some("invalid_target_budget"),
            Some(target) if spent >= target => Some("target_budget_reached"),
            Some(_) => None,
        },
    }
}

fn manually_unassigned(doc: &ContextDoc, state: &DispatchState, actor: &str) -> bool {
    state.last_dispatch_task.get(actor).is_some_and(|id| {
        doc.tasks.iter().any(|task| {
            text(task, "id") == id
                && match task.get("assignee") {
                    None | Some(Value::Null) => true,
                    Some(Value::String(s)) => s.trim().is_empty(),
                    Some(_) => false,
                }
        })
    })
}
fn cooldown_ready(
    doc: &ContextDoc,
    state: &DispatchState,
    actor: &str,
    now: i64,
    cooldown: u64,
) -> bool {
    manually_unassigned(doc, state, actor)
        || state.last_dispatch_ms.get(actor).is_none_or(|last| {
            now.checked_sub(*last)
                .is_some_and(|elapsed| elapsed >= 0 && elapsed as u64 >= cooldown)
        })
}

/// Read-only snapshot of the same predicates used at assignment. Eligibility
/// can change after a read; the committing transaction always checks again.
pub fn task_block_reasons(
    home: &HomeLayout,
    group: &str,
    doc: &ContextDoc,
    actors: &[DispatchActor],
    cooldown: u64,
    now: i64,
) -> io::Result<BTreeMap<String, Option<&'static str>>> {
    let path = crate::GroupStore::new(home.clone())?
        .state_dir(group)?
        .join("task-dispatch.json");
    let state: DispatchState = if path.try_exists()? {
        crate::fs::read_json(&path)?
    } else {
        DispatchState::default()
    };
    // The document is immutable for this read. Compute Actor-level exclusions
    // once, not a full task scan for every task/Actor pair on a paged read.
    let availability = actors
        .iter()
        .map(|actor| {
            (
                actor,
                !holds_open_task(doc, &actor.actor_id),
                cooldown_ready(doc, &state, &actor.actor_id, now, cooldown),
            )
        })
        .collect::<Vec<_>>();
    Ok(doc
        .tasks
        .iter()
        .map(|task| {
            let reason = ready_reason(doc, task).or_else(|| {
                if actors.is_empty() {
                    return Some("no_idle_actor");
                }
                let fitting = availability
                    .iter()
                    .filter(|entry| fits(task, entry.0))
                    .collect::<Vec<_>>();
                if fitting.is_empty() {
                    return Some("no_budget_fitting_actor");
                }
                let available = fitting
                    .into_iter()
                    .filter(|entry| entry.1)
                    .collect::<Vec<_>>();
                if available.is_empty() {
                    return Some("actor_holds_open_task");
                }
                if !available.into_iter().any(|entry| entry.2) {
                    return Some("actor_cooldown");
                }
                None
            });
            (text(task, "id").to_owned(), reason)
        })
        .collect())
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
