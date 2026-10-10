use cccc_contracts::{Event, GroupState};
use chrono::{DateTime, Utc};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;

use crate::actors;
use crate::automation_render::notify_events;
use crate::automation_schedule::is_due;
use crate::{GroupDoc, GroupStore, HomeLayout, inbox, ledger};

mod state;
use state::RuntimeState;

pub const STANDUP_SNIPPET: &str = "{{interval_minutes}} minutes have passed. Stand-up checkpoint (foreman only).\n\nUse MCP chat for any visible update. Keep this short.";

#[derive(Debug, Clone)]
pub enum ScheduledAction {
    GroupState {
        group_id: String,
        state: String,
        rule_id: String,
        fired_at: i64,
        one_time: bool,
    },
    ActorControl {
        group_id: String,
        operation: String,
        targets: Vec<String>,
        rule_id: String,
        fired_at: i64,
        one_time: bool,
    },
}

#[derive(Debug, Default)]
pub struct TickResult {
    pub notifications: Vec<Event>,
    pub actions: Vec<ScheduledAction>,
}

pub fn tick(home: &HomeLayout) -> io::Result<TickResult> {
    tick_scheduled(home, true)
}

pub fn tick_scheduled(home: &HomeLayout, include_unread: bool) -> io::Result<TickResult> {
    let mut result = TickResult::default();
    for group_id in group_ids(home)? {
        let group_result = match tick_group(home, &group_id, include_unread) {
            Ok(result) => result,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        result.notifications.extend(group_result.notifications);
        result.actions.extend(group_result.actions);
    }
    Ok(result)
}

pub fn group_ids(home: &HomeLayout) -> io::Result<Vec<String>> {
    let store = GroupStore::new(home.clone())?;
    Ok(store
        .list()?
        .into_iter()
        .map(|group| group.group_id)
        .collect())
}

pub fn reconcile_rule_state(
    store: &GroupStore,
    group_id: &str,
    previous: &[Value],
    current: &[Value],
) -> io::Result<()> {
    state::reconcile_rules(store, group_id, previous, current)
}

pub fn mark_rule_fired(
    home: &HomeLayout,
    group_id: &str,
    rule_id: &str,
    fired_at: i64,
) -> io::Result<()> {
    let store = GroupStore::new(home.clone())?;
    let mut state = state::load(&store, group_id)?;
    state.last_rule.insert(rule_id.to_owned(), fired_at);
    state::save(&store, group_id, &state)
}

pub fn next_rule_fire_at(
    trigger: Option<&Map<String, Value>>,
    last: Option<i64>,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    crate::automation_schedule::next_fire_at(trigger, last, now)
}

/// Validate a cron trigger expression, returning the parse error text on failure.
/// Day-of-week fields are interpreted with POSIX numbering (0 or 7 = Sunday).
pub fn cron_expression_error(raw: &str) -> Option<String> {
    crate::automation_schedule::parse_cron_schedule(raw)
        .err()
        .map(|error| error.to_string())
}

pub fn reset_rule_timers_on_resume(home: &HomeLayout, group_id: &str) -> io::Result<()> {
    let store = GroupStore::new(home.clone())?;
    let group = store.load(group_id)?;
    let Some(rules) = group.automation.get("rules").and_then(Value::as_array) else {
        return Ok(());
    };
    let now = Utc::now();
    let mut state = state::load(&store, group_id)?;
    let previous = state.clone();
    for rule in rules.iter().filter_map(Value::as_object) {
        if rule.get("enabled").and_then(Value::as_bool) == Some(false) {
            continue;
        }
        let id = rule.get("id").and_then(Value::as_str).unwrap_or("");
        if id.is_empty() {
            continue;
        }
        let trigger = rule.get("trigger").and_then(Value::as_object);
        let kind = trigger
            .and_then(|trigger| trigger.get("kind"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let should_reset = match kind {
            "interval" | "cron" => true,
            "at" => trigger
                .and_then(|trigger| trigger.get("at"))
                .and_then(Value::as_str)
                .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                .is_some_and(|scheduled| scheduled <= now),
            _ => false,
        };
        if should_reset {
            state.last_rule.insert(id.to_owned(), now.timestamp());
        }
    }
    if state != previous {
        state::save(&store, group_id, &state)?;
    }
    Ok(())
}

pub fn tick_group(
    home: &HomeLayout,
    group_id: &str,
    include_unread: bool,
) -> io::Result<TickResult> {
    tick_group_inner(home, group_id, include_unread, None, None)
}

pub fn tick_group_for_delivery_actors(
    home: &HomeLayout,
    group_id: &str,
    include_unread: bool,
    delivery_actor_ids: &HashSet<String>,
) -> io::Result<TickResult> {
    tick_group_inner(
        home,
        group_id,
        include_unread,
        Some(delivery_actor_ids),
        None,
    )
}

/// Like [`tick_group_for_delivery_actors`], also giving each actor with a
/// running managed session whether that session reports an ended turn
/// (`true`) or is busy (`false`). Only the opt-in `delivery.mail_wake_on_idle`
/// and `delivery.task_wake_on_idle` policies read `managed_idle`.
pub fn tick_group_for_delivery_actors_with_managed_idle(
    home: &HomeLayout,
    group_id: &str,
    include_unread: bool,
    delivery_actor_ids: &HashSet<String>,
    managed_idle: &HashMap<String, bool>,
) -> io::Result<TickResult> {
    tick_group_inner(
        home,
        group_id,
        include_unread,
        Some(delivery_actor_ids),
        Some(managed_idle),
    )
}

/// Whether the group opted into notifying idle managed sessions about Mail.
#[must_use]
pub fn mail_wake_on_idle(group: &GroupDoc) -> bool {
    group
        .extra
        .get("delivery")
        .and_then(|delivery| delivery.get("mail_wake_on_idle"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Whether the group opted into reminding idle managed sessions about the
/// active cards assigned to them.
#[must_use]
pub fn task_wake_on_idle(group: &GroupDoc) -> bool {
    group
        .extra
        .get("delivery")
        .and_then(|delivery| delivery.get("task_wake_on_idle"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn tick_group_inner(
    home: &HomeLayout,
    group_id: &str,
    include_unread: bool,
    delivery_actor_ids: Option<&HashSet<String>>,
    managed_idle: Option<&HashMap<String, bool>>,
) -> io::Result<TickResult> {
    let store = GroupStore::new(home.clone())?;
    let mut result = TickResult::default();
    let group = store.load(group_id)?;
    if matches!(group.state, GroupState::Paused | GroupState::Stopped) {
        return Ok(result);
    }
    let mut state = state::load(&store, group_id)?;
    let previous = state.clone();
    tick_rules(&store, &group, &mut state, &mut result)?;
    if include_unread && matches!(group.state, GroupState::Active | GroupState::Idle) {
        tick_unread(
            home,
            &store,
            &group,
            delivery_actor_ids,
            managed_idle,
            &mut result,
        )?;
    }
    if state != previous {
        state::save(&store, group_id, &state)?;
    }
    Ok(result)
}

fn tick_rules(
    store: &GroupStore,
    group: &GroupDoc,
    state: &mut RuntimeState,
    result: &mut TickResult,
) -> io::Result<()> {
    let Some(rules) = group.automation.get("rules").and_then(Value::as_array) else {
        return Ok(());
    };
    let now = Utc::now();
    for rule in rules.iter().filter_map(Value::as_object) {
        if rule.get("enabled").and_then(Value::as_bool) == Some(false) {
            continue;
        }
        let id = rule.get("id").and_then(Value::as_str).unwrap_or("");
        if id.is_empty() {
            continue;
        }
        if group.state == GroupState::Idle && id == "standup" {
            continue;
        }
        let trigger = rule.get("trigger").and_then(Value::as_object);
        let last_fired = state.last_rule.get(id).copied();
        let trigger_kind = trigger
            .and_then(|trigger| trigger.get("kind"))
            .and_then(Value::as_str)
            .unwrap_or("interval");
        if trigger_kind == "interval" && last_fired.is_none() {
            state.last_rule.insert(id.into(), now.timestamp());
            continue;
        }
        if !is_due(trigger, last_fired, now) {
            continue;
        }
        let action = rule.get("action").and_then(Value::as_object);
        let kind = action
            .and_then(|action| action.get("kind"))
            .and_then(Value::as_str)
            .unwrap_or("notify");
        let one_time = trigger_kind == "at";
        let mut completed = false;
        match kind {
            "notify" => {
                let scheduled_at = scheduled_at(trigger, last_fired, now);
                let events = notify_events(group, id, rule, action, &scheduled_at);
                for event in events {
                    ledger::append(&store.ledger_path(&group.group_id)?, &event)?;
                    result.notifications.push(event);
                    completed = true;
                }
            }
            "group_state" => {
                if let Some(target) = action
                    .and_then(|action| action.get("state"))
                    .and_then(Value::as_str)
                {
                    result.actions.push(ScheduledAction::GroupState {
                        group_id: group.group_id.clone(),
                        state: target.into(),
                        rule_id: id.into(),
                        fired_at: now.timestamp(),
                        one_time,
                    });
                }
            }
            "actor_control" => {
                let operation = action
                    .and_then(|action| action.get("operation"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let targets = action
                    .and_then(|action| action.get("targets"))
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect();
                if !operation.is_empty() {
                    result.actions.push(ScheduledAction::ActorControl {
                        group_id: group.group_id.clone(),
                        operation: operation.into(),
                        targets,
                        rule_id: id.into(),
                        fired_at: now.timestamp(),
                        one_time,
                    });
                }
            }
            _ => {}
        }
        if completed {
            state.last_rule.insert(id.into(), now.timestamp());
        }
    }
    Ok(())
}

fn scheduled_at(
    trigger: Option<&serde_json::Map<String, Value>>,
    last_fired: Option<i64>,
    now: DateTime<Utc>,
) -> String {
    let kind = trigger
        .and_then(|trigger| trigger.get("kind"))
        .and_then(Value::as_str)
        .unwrap_or("interval");
    let timestamp = match kind {
        "interval" => last_fired
            .zip(
                trigger
                    .and_then(|trigger| trigger.get("every_seconds"))
                    .and_then(Value::as_i64),
            )
            .and_then(|(last, seconds)| DateTime::from_timestamp(last + seconds, 0)),
        "at" => trigger
            .and_then(|trigger| trigger.get("at"))
            .and_then(Value::as_str)
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&Utc)),
        "cron" => DateTime::from_timestamp(now.timestamp().div_euclid(60) * 60, 0),
        _ => None,
    };
    timestamp.map_or_else(String::new, |value| value.to_rfc3339())
}

fn tick_unread(
    home: &HomeLayout,
    store: &GroupStore,
    group: &GroupDoc,
    delivery_actor_ids: Option<&HashSet<String>>,
    managed_idle: Option<&HashMap<String, bool>>,
    result: &mut TickResult,
) -> io::Result<()> {
    let mail_after = delivery_timing_value(group, "mail_notice_after_seconds", 1_800);
    let reply_after = delivery_timing_value(group, "reply_notice_after_seconds", 900);
    let no_managed_sessions = HashMap::new();
    let managed_idle = managed_idle.unwrap_or(&no_managed_sessions);
    let wake = mail_wake_on_idle(group).then(|| MailWake {
        min_age: delivery_timing_value(group, "mail_wake_min_age_seconds", 60),
        managed_idle,
    });
    let card_wake = task_wake_on_idle(group);
    if mail_after <= 0 && reply_after <= 0 && wake.is_none() && !card_wake {
        return Ok(());
    }
    let eligible = actors::visible(group)
        .filter(|actor| {
            actor.enabled && delivery_actor_ids.is_none_or(|ids| ids.contains(&actor.id))
        })
        .collect::<Vec<_>>();
    if eligible.is_empty() {
        return Ok(());
    }
    // Cards are read only when an eligible managed session reports idle.
    let held_cards = if card_wake
        && eligible
            .iter()
            .any(|actor| managed_idle.get(&actor.id) == Some(&true))
    {
        Some(assigned_cards(home, &group.group_id)?)
    } else {
        None
    };
    let ledger_path = store.ledger_path(&group.group_id)?;
    let cursors = inbox::cursors(home, &group.group_id)?;
    // Project only the notices while borrowing history. Release the index read
    // lock before appending, since append also updates that same index.
    let notices = ledger::inspect(&ledger_path, |events, positions| {
        let mut notices = unread_notices(
            group,
            &eligible,
            events,
            positions,
            &cursors,
            &NoticePolicy {
                mail_after,
                reply_after,
                wake,
            },
        );
        if let Some(held_cards) = &held_cards {
            notices.extend(card_notices(
                group,
                &eligible,
                events,
                held_cards,
                managed_idle,
                delivery_timing_value(group, "task_wake_interval_seconds", 1_800).max(60),
            ));
        }
        notices
    })?;
    for event in notices {
        ledger::append(&ledger_path, &event)?;
        result.notifications.push(event);
    }
    Ok(())
}

fn unread_notices(
    group: &GroupDoc,
    eligible: &[&cccc_contracts::Actor],
    events: &[Event],
    positions: &HashMap<String, usize>,
    cursors: &BTreeMap<String, String>,
    policy: &NoticePolicy<'_>,
) -> Vec<Event> {
    let NoticePolicy {
        mail_after,
        reply_after,
        ref wake,
    } = *policy;
    let wake = wake.as_ref();
    let mut notices = Vec::new();
    let generations = inbox::actor_generation_positions(events);
    let now = Utc::now().timestamp();
    let resume_at = events.iter().rev().find_map(|event| {
        let resumed = event.kind == "group.start"
            || (event.kind == "group.set_state"
                && matches!(
                    event.data.get("new_state").and_then(Value::as_str),
                    Some("active" | "idle")
                ));
        resumed.then(|| timestamp(&event.ts, now))
    });

    let mut mail_reads = HashMap::<String, Vec<(usize, usize)>>::new();
    let mut replies = HashMap::<(String, String), usize>::new();
    let mut cancelled = HashSet::<String>::new();
    let mut deliveries = HashMap::<(String, String), (String, i64, usize)>::new();
    let mut actor_resumes = HashMap::<String, i64>::new();
    let mut mail_claims = HashMap::<(String, String), Vec<Vec<String>>>::new();
    let mut reply_claims = HashSet::<(String, String, String)>::new();
    for (position, event) in events.iter().enumerate() {
        match event.kind.as_str() {
            "actor.start" | "actor.restart" | "actor.new_session" => {
                if let Some(actor_id) = event.data.get("actor_id").and_then(Value::as_str) {
                    actor_resumes.insert(actor_id.to_owned(), timestamp(&event.ts, now));
                }
            }
            "mail.read" => {
                if let (Some(actor_id), Some(boundary_id)) = (
                    event.data.get("actor_id").and_then(Value::as_str),
                    event.data.get("event_id").and_then(Value::as_str),
                ) && let Some(boundary_position) = positions.get(boundary_id)
                {
                    mail_reads
                        .entry(actor_id.to_owned())
                        .or_default()
                        .push((position, *boundary_position));
                }
            }
            "chat.message" => {
                if let Some(source) = event.data.get("reply_to").and_then(Value::as_str) {
                    replies
                        .entry((source.to_owned(), event.by.clone()))
                        .or_insert(position);
                }
            }
            "chat.reply_request.cancelled" => {
                if let Some(source) = event.data.get("source_event_id").and_then(Value::as_str) {
                    cancelled.insert(source.to_owned());
                }
            }
            "runtime.delivery" => {
                if let (Some(source), Some(actor_id), Some(state)) = (
                    event.data.get("source_event_id").and_then(Value::as_str),
                    event.data.get("actor_id").and_then(Value::as_str),
                    event.data.get("state").and_then(Value::as_str),
                ) {
                    deliveries.insert(
                        (source.to_owned(), actor_id.to_owned()),
                        (state.to_owned(), timestamp(&event.ts, now), position),
                    );
                }
            }
            "system.notify" => {
                let kind = event
                    .data
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !matches!(kind, "mail_notice" | "reply_notice") {
                    continue;
                }
                let context = event.data.get("context").and_then(Value::as_object);
                let actor_id = context
                    .and_then(|value| value.get("actor_id"))
                    .and_then(Value::as_str)
                    .or_else(|| event.data.get("target_actor_id").and_then(Value::as_str))
                    .unwrap_or_default();
                let created_at = context
                    .and_then(|value| value.get("actor_created_at"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let source_ids = context
                    .and_then(|value| value.get("source_event_ids"))
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect::<Vec<_>>();
                if kind == "mail_notice" {
                    mail_claims
                        .entry((actor_id.into(), created_at.into()))
                        .or_default()
                        .push(source_ids);
                } else {
                    reply_claims.extend(
                        source_ids
                            .into_iter()
                            .map(|source| (actor_id.into(), created_at.into(), source)),
                    );
                }
            }
            _ => {}
        }
    }

    let resolution_position = |actor_id: &str, source_event_id: &str| {
        let source_position = positions.get(source_event_id).copied()?;
        let read_position = mail_reads.get(actor_id).and_then(|facts| {
            facts.iter().find_map(|(fact_position, boundary_position)| {
                (*boundary_position >= source_position).then_some(*fact_position)
            })
        });
        let reply_position = replies
            .get(&(source_event_id.to_owned(), actor_id.to_owned()))
            .copied();
        let delivery_position = deliveries
            .get(&(source_event_id.to_owned(), actor_id.to_owned()))
            .and_then(|(state, _, fact_position)| {
                matches!(state.as_str(), "accepted" | "ambiguous").then_some(*fact_position)
            });
        [read_position, reply_position, delivery_position]
            .into_iter()
            .flatten()
            .min()
    };

    for actor in eligible {
        let generation = generations.get(&actor.id).copied().unwrap_or(0);
        let cursor_position = cursors
            .get(&actor.id)
            .and_then(|event_id| positions.get(event_id))
            .copied();
        let mut mail_pending = Vec::<&Event>::new();
        let mut reply_due = Vec::<&Event>::new();
        for (position, source) in events.iter().enumerate().skip(generation) {
            if source.kind != "chat.message"
                || source.by == actor.id
                || !inbox::is_for_actor(group, source, &actor.id)
            {
                continue;
            }
            let mode = source
                .data
                .get("message_mode")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let read = cursor_position.is_some_and(|cursor| cursor >= position);
            let replied = replies.contains_key(&(source.id.clone(), actor.id.clone()));
            let delivery = deliveries.get(&(source.id.clone(), actor.id.clone()));
            if mode == "mail" {
                let recipients = source
                    .data
                    .get("to")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>();
                let broadcast_like = recipients.is_empty()
                    || recipients
                        .iter()
                        .any(|recipient| matches!(*recipient, "@all" | "@peers" | "@foreman"));
                if !broadcast_like
                    && !read
                    && !replied
                    && !delivery.is_some_and(|(state, _, _)| {
                        matches!(state.as_str(), "accepted" | "ambiguous")
                    })
                {
                    mail_pending.push(source);
                }
                continue;
            }
            if mode != "request_reply"
                || replied
                || cancelled.contains(&source.id)
                || reply_claims.contains(&(
                    actor.id.clone(),
                    actor.created_at.clone(),
                    source.id.clone(),
                ))
            {
                continue;
            }
            let Some(started_at) = delivery
                .filter(|(state, _, _)| state == "accepted")
                .map(|(_, at, _)| *at)
            else {
                continue;
            };
            let started_at = resume_at.map_or(started_at, |resume| started_at.max(resume));
            if reply_after > 0 && now - started_at >= reply_after {
                reply_due.push(source);
            }
        }

        let managed_idle = wake.and_then(|wake| Some((wake, *wake.managed_idle.get(&actor.id)?)));
        if let Some((wake, idle)) = managed_idle {
            // A busy managed session is notified once its turn ends.
            if idle {
                let claims = mail_claims
                    .get(&(actor.id.clone(), actor.created_at.clone()))
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                // Notices from before claims named their sources keep the
                // original batch latch.
                let unknown_claim = claims
                    .iter()
                    .any(|claimed| !claimed.iter().any(|source| positions.contains_key(source)));
                let claimed = claims
                    .iter()
                    .flatten()
                    .map(String::as_str)
                    .collect::<HashSet<_>>();
                let actor_resume = actor_resumes.get(&actor.id).copied();
                if !unknown_claim {
                    notices.extend(idle_wake_notice(
                        group,
                        actor,
                        &mail_pending,
                        &claimed,
                        wake.min_age,
                        [resume_at, actor_resume],
                        now,
                    ));
                }
            }
        } else if mail_after > 0 && !mail_pending.is_empty() {
            let pending_ids = mail_pending
                .iter()
                .map(|event| event.id.clone())
                .collect::<HashSet<_>>();
            let active_claim = mail_claims
                .get(&(actor.id.clone(), actor.created_at.clone()))
                .into_iter()
                .flatten()
                .rev()
                .any(|claimed| {
                    let claimed = claimed
                        .iter()
                        .filter(|source| positions.contains_key(*source))
                        .collect::<Vec<_>>();
                    if claimed.is_empty() {
                        return true;
                    }
                    let resolutions = claimed
                        .iter()
                        .map(|source| resolution_position(&actor.id, source))
                        .collect::<Vec<_>>();
                    if resolutions.iter().any(Option::is_none) {
                        return true;
                    }
                    let closure_position = resolutions.into_iter().flatten().max().unwrap_or(0);
                    pending_ids.iter().any(|source| {
                        positions
                            .get(source)
                            .is_some_and(|position| *position <= closure_position)
                    })
                });
            let first_at = timestamp(&mail_pending[0].ts, now);
            let first_at = resume_at.map_or(first_at, |resume| first_at.max(resume));
            let first_at = actor_resumes
                .get(&actor.id)
                .map_or(first_at, |resume| first_at.max(*resume));
            if !active_claim && now - first_at >= mail_after {
                let event = notice_event(
                    group,
                    actor,
                    "mail_notice",
                    "Mail waiting",
                    &format!(
                        "You have {} Mail item(s) waiting. Call cccc_inbox_read when appropriate.",
                        mail_pending.len()
                    ),
                    mail_pending.iter().map(|event| event.id.clone()).collect(),
                );
                notices.push(event);
            }
        }
        if !reply_due.is_empty() {
            let event = notice_event(
                group,
                actor,
                "reply_notice",
                "Reply requested",
                &format!(
                    "{} message(s) still need a concrete reply. Use cccc_message_history if needed, then cccc_message_reply.",
                    reply_due.len()
                ),
                reply_due.iter().map(|event| event.id.clone()).collect(),
            );
            notices.push(event);
        }
    }
    notices
}

/// Mail and reply notice timing for one unread tick.
struct NoticePolicy<'a> {
    mail_after: i64,
    reply_after: i64,
    wake: Option<MailWake<'a>>,
}

/// Opt-in `delivery.mail_wake_on_idle` policy for one tick.
#[derive(Clone, Copy)]
struct MailWake<'a> {
    min_age: i64,
    /// Whether each actor's running managed session reports an ended turn.
    /// This is the runtime's own turn signal, never an inferred idle
    /// heuristic; actors without one keep the ordinary notice timer.
    managed_idle: &'a HashMap<String, bool>,
}

/// One content-free notice per new Mail batch for an idle managed session.
/// Mail an earlier notice already claimed is never claimed again, so an
/// ignored notice cannot repeat, but it no longer holds back notices for
/// later Mail.
fn idle_wake_notice(
    group: &GroupDoc,
    actor: &cccc_contracts::Actor,
    mail_pending: &[&Event],
    claimed: &HashSet<&str>,
    min_age: i64,
    resumes: [Option<i64>; 2],
    now: i64,
) -> Option<Event> {
    // Each source must itself be `min_age` old, counted from the latest
    // group or session start; younger Mail waits for a later notice.
    let floor = resumes.into_iter().flatten().max();
    let aged = mail_pending
        .iter()
        .filter(|event| !claimed.contains(event.id.as_str()))
        .filter(|event| {
            let at = timestamp(&event.ts, now).max(floor.unwrap_or(i64::MIN));
            now - at >= min_age
        })
        .map(|event| event.id.clone())
        .collect::<Vec<_>>();
    (!aged.is_empty()).then(|| {
        notice_event(
            group,
            actor,
            "mail_notice",
            "Mail waiting",
            &format!(
                "You have {} Mail item(s) waiting. Call cccc_inbox_read when appropriate.",
                mail_pending.len()
            ),
            aged,
        )
    })
}

/// Active cards each actor is the assignee of, excluding blocked cards and
/// cards waiting on the user, an actor, or an external party.
fn assigned_cards(home: &HomeLayout, group_id: &str) -> io::Result<HashMap<String, Vec<String>>> {
    let context = crate::context::ContextStore::new(home.clone())?.load(group_id)?;
    let text = |task: &Map<String, Value>, key: &str| {
        task.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned()
    };
    let mut held = HashMap::<String, Vec<String>>::new();
    for task in &context.tasks {
        let blocked = task
            .get("blocked_by")
            .and_then(Value::as_array)
            .is_some_and(|items| !items.is_empty());
        let assignee = text(task, "assignee");
        if text(task, "status") != "active"
            || assignee.is_empty()
            || blocked
            || matches!(
                text(task, "waiting_on").as_str(),
                "user" | "actor" | "external"
            )
        {
            continue;
        }
        held.entry(assignee).or_default().push(text(task, "id"));
    }
    Ok(held)
}

/// One content-free reminder to an idle managed session that holds active,
/// unblocked cards and has been quiet for `interval`. Quiet means no message
/// from the actor, no session start, and no earlier card reminder in that
/// window, so a working lane is never prompted and an idle one at most once
/// per `interval`.
fn card_notices(
    group: &GroupDoc,
    eligible: &[&cccc_contracts::Actor],
    events: &[Event],
    held_cards: &HashMap<String, Vec<String>>,
    managed_idle: &HashMap<String, bool>,
    interval: i64,
) -> Vec<Event> {
    let now = Utc::now().timestamp();
    // Newest timestamp per actor, not the last ledger position: an
    // out-of-order event must not reopen a window a later one closed.
    let mut last_activity = HashMap::<&str, i64>::new();
    // A group start or resume restarts every actor's quiet window, as it
    // does the Mail notice window.
    let mut group_resume: Option<i64> = None;
    for event in events {
        let resumed = event.kind == "group.start"
            || (event.kind == "group.set_state"
                && matches!(
                    event.data.get("new_state").and_then(Value::as_str),
                    Some("active" | "idle")
                ));
        if resumed {
            let at = timestamp(&event.ts, now);
            group_resume = Some(group_resume.map_or(at, |seen| seen.max(at)));
            continue;
        }
        let actor_id = match event.kind.as_str() {
            "chat.message" => Some(event.by.as_str()),
            "actor.start" | "actor.restart" | "actor.new_session" => {
                event.data.get("actor_id").and_then(Value::as_str)
            }
            "system.notify"
                if event.data.get("kind").and_then(Value::as_str) == Some("task_notice") =>
            {
                event.data.get("target_actor_id").and_then(Value::as_str)
            }
            _ => None,
        };
        if let Some(actor_id) = actor_id {
            let at = timestamp(&event.ts, now);
            last_activity
                .entry(actor_id)
                .and_modify(|seen| *seen = (*seen).max(at))
                .or_insert(at);
        }
    }
    eligible
        .iter()
        .filter(|actor| managed_idle.get(&actor.id) == Some(&true))
        .filter(|actor| {
            last_activity
                .get(actor.id.as_str())
                .copied()
                .max(group_resume)
                .is_none_or(|at| now - at >= interval)
        })
        .filter_map(|actor| {
            let cards = held_cards.get(&actor.id).filter(|cards| !cards.is_empty())?;
            let mut event = Event::new("system.notify", &group.group_id);
            event.by = "system".into();
            event.data = json!({
                "kind":"task_notice",
                "priority":"normal",
                "title":"Cards waiting",
                "message":format!(
                    "You are the assignee of {} active card(s): {}. Continue the next step, or update each card with cccc_task if it is done, blocked, or waiting on someone.",
                    cards.len(),
                    cards.join(", ")
                ),
                "target_actor_id":actor.id,
                "im_visibility":"internal",
                "context":{
                    "actor_id":actor.id,
                    "actor_created_at":actor.created_at,
                    "task_ids":cards,
                    "count":cards.len(),
                },
            })
            .as_object()
            .cloned()
            .expect("card reminder data");
            Some(event)
        })
        .collect()
}

fn delivery_timing_value(group: &GroupDoc, key: &str, default: i64) -> i64 {
    group
        .extra
        .get("delivery")
        .and_then(Value::as_object)
        .and_then(|delivery| delivery.get(key))
        .and_then(Value::as_i64)
        .unwrap_or(default)
        .max(0)
}

fn timestamp(value: &str, default: i64) -> i64 {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.timestamp())
        .unwrap_or(default)
}

fn notice_event(
    group: &GroupDoc,
    actor: &cccc_contracts::Actor,
    kind: &str,
    title: &str,
    message: &str,
    source_event_ids: Vec<String>,
) -> Event {
    let mut event = Event::new("system.notify", &group.group_id);
    event.by = "system".into();
    event.data = json!({
        "kind":kind,
        "priority":"normal",
        "title":title,
        "message":message,
        "target_actor_id":actor.id,
        "related_event_id":source_event_ids.first().cloned().unwrap_or_default(),
        "im_visibility":"internal",
        "context":{
            "actor_id":actor.id,
            "actor_created_at":actor.created_at,
            "source_event_ids":source_event_ids,
            "count":source_event_ids.len(),
        },
    })
    .as_object()
    .cloned()
    .expect("reminder data");
    event
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unread_tick_without_eligible_recipients_does_not_load_history() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path()).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let mut group = store.create("idle cost", "").expect("group");
        group.state = GroupState::Active;
        let path = store.ledger_path(&group.group_id).expect("ledger");
        ledger::append(&path, &Event::new("chat.message", &group.group_id)).expect("append");
        for mode in ["no actors", "disabled", "not running"] {
            if mode != "no actors" {
                let mut actor = cccc_contracts::Actor::new("peer");
                actor.enabled = mode != "disabled";
                group.actors = vec![actor];
            }
            store.save(&group).expect("save");
            crate::ledger_index::invalidate_path(&path);
            let result = if mode == "not running" {
                tick_group_for_delivery_actors(&home, &group.group_id, true, &HashSet::new())
            } else {
                tick_group(&home, &group.group_id, true)
            }
            .expect("tick");
            assert!(result.notifications.is_empty());
            assert!(!crate::ledger_index::is_cached(&path), "{mode}");
        }
    }
}
