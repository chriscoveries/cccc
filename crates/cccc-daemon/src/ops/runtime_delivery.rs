use cccc_contracts::{Actor, Event};
use cccc_core::fs::with_exclusive_lock;
use cccc_core::{GroupDoc, GroupStore, HomeLayout, inbox, ledger};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::dispatch::OpError;

mod legacy_read_watermark;

use legacy_read_watermark::LegacyReadWatermark;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimResult {
    Claimed,
    Terminal(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryOutcome<'a> {
    Claimed,
    Accepted,
    Failed(&'a str),
    Ambiguous(&'a str),
    /// Never sent and not to be retried: a newer event supersedes it.
    Withdrawn(&'a str),
}

impl<'a> DeliveryOutcome<'a> {
    fn parts(self) -> (&'static str, &'a str) {
        match self {
            Self::Claimed => ("claimed", ""),
            Self::Accepted => ("accepted", ""),
            Self::Failed(reason) => ("failed", reason),
            Self::Ambiguous(reason) => ("ambiguous", reason),
            Self::Withdrawn(reason) => ("withdrawn", reason),
        }
    }
}

pub fn delivery_id(
    group_id: &str,
    actor_id: &str,
    actor_created_at: &str,
    source_event_id: &str,
) -> String {
    let seed = [group_id, actor_id, actor_created_at, source_event_id].join("\0");
    let digest = format!("{:x}", Sha256::digest(seed.as_bytes()));
    format!("delivery:{actor_id}:{}", &digest[..24])
}

pub fn append_state(
    home: &HomeLayout,
    group_id: &str,
    actor_id: &str,
    actor_created_at: &str,
    source_event_id: &str,
    transport: &str,
    outcome: DeliveryOutcome<'_>,
) -> Result<Event, OpError> {
    let store = GroupStore::new(home.clone()).map_err(OpError::io)?;
    let (state, reason) = outcome.parts();
    let mut event = Event::new("runtime.delivery", group_id);
    event.by = "system".into();
    event.data = json!({
        "actor_id":actor_id,
        "source_event_id":source_event_id,
        "delivery_id":delivery_id(group_id, actor_id, actor_created_at, source_event_id),
        "state":state,
        "transport":transport,
        "reason":if reason.is_empty() { Value::Null } else { Value::String(reason.into()) },
    })
    .as_object()
    .cloned()
    .expect("runtime delivery data");
    ledger::append(&store.ledger_path(group_id).map_err(OpError::io)?, &event)
        .map_err(OpError::io)?;
    Ok(event)
}

/// Ledger event naming the process a delivery was handed to. Written before the
/// payload reaches the terminal so a misroute stays reconstructible afterwards.
pub const LAUNCH_EVENT: &str = "runtime.delivery.launch";

/// Where a delivery was handed over: the process that received it.
///
/// The daemon keeps no launch record of its own, so after a misroute the ledger
/// could not distinguish "the launch spec carried another actor's id" from "the
/// lane was started outside the daemon with that env exported". One record per
/// distinct process closes that gap without relying on the journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchIdentity {
    /// None only when the runtime cannot report a pid; the line is still
    /// written, because the started_at and session key still identify the
    /// handover.
    pub pid: Option<u32>,
    pub started_at: String,
    pub session_key: String,
    pub runner: &'static str,
}

impl LaunchIdentity {
    pub fn new(
        group_id: &str,
        actor_id: &str,
        pid: Option<u32>,
        started_at: &str,
        runner: &'static str,
    ) -> Self {
        Self {
            pid,
            started_at: started_at.to_owned(),
            session_key: runtime_session_key(group_id, actor_id),
            runner,
        }
    }
}

/// Sessions are keyed by (group, actor) in the runtime registry; record that
/// pair, so that two sessions of one actor are told apart by `started_at`.
pub fn runtime_session_key(group_id: &str, actor_id: &str) -> String {
    format!("{group_id}:{actor_id}")
}

fn launch_data(
    group_id: &str,
    actor_id: &str,
    actor_created_at: &str,
    source_event_id: &str,
    transport: &str,
    identity: &LaunchIdentity,
) -> Map<String, Value> {
    json!({
        "actor_id": actor_id,
        "source_event_id": source_event_id,
        "delivery_id": delivery_id(group_id, actor_id, actor_created_at, source_event_id),
        "pid": identity.pid,
        "started_at": identity.started_at,
        "session_key": identity.session_key,
        "runner": identity.runner,
        "state": "launched",
        "transport": transport,
    })
    .as_object()
    .cloned()
    .expect("runtime delivery launch data")
}

/// Record the process a delivery is about to reach.
///
/// Idempotent per distinct process: a retry that lands on the same pid and
/// started_at writes nothing, while a relaunch writes its own line, so every
/// attempt stays visible. The line is read back inside the same exclusive lock
/// that guards the claim, and the caller must not write to the terminal unless
/// this returns a recorded event.
pub fn record_launch(
    home: &HomeLayout,
    group_id: &str,
    actor_id: &str,
    actor_created_at: &str,
    source_event_id: &str,
    transport: &str,
    identity: &LaunchIdentity,
) -> Result<Event, OpError> {
    let store = GroupStore::new(home.clone()).map_err(OpError::io)?;
    let ledger_path = store.ledger_path(group_id).map_err(OpError::io)?;
    let data = launch_data(
        group_id,
        actor_id,
        actor_created_at,
        source_event_id,
        transport,
        identity,
    );
    with_exclusive_lock(&delivery_lock_path(&ledger_path), || {
        let already = ledger::inspect(&ledger_path, |events, _| {
            events.iter().any(|event| {
                event.kind == LAUNCH_EVENT
                    && event.data.get("delivery_id") == data.get("delivery_id")
                    && event.data.get("pid") == data.get("pid")
                    && event.data.get("started_at") == data.get("started_at")
            })
        })?;
        if already {
            let mut event = Event::new(LAUNCH_EVENT, group_id);
            event.by = "system".into();
            event.data = data.clone();
            return Ok(event);
        }
        let mut event = Event::new(LAUNCH_EVENT, group_id);
        event.by = "system".into();
        event.data = data;
        ledger::append(&ledger_path, &event)?;
        let recorded = ledger::inspect(&ledger_path, |events, _| {
            events.iter().any(|appended| appended.id == event.id)
        })?;
        if !recorded {
            // The payload must not be written on an unrecorded handover.
            return Err(std::io::Error::other(
                "launch record was not readable after it was appended",
            ));
        }
        Ok(event)
    })
    .map_err(OpError::io)
}

/// The most recent recorded handover of a delivery, or None for deliveries that
/// were never handed to a process.
pub fn launch_record(
    home: &HomeLayout,
    group_id: &str,
    actor_id: &str,
    actor_created_at: &str,
    source_event_id: &str,
) -> Result<Option<Map<String, Value>>, OpError> {
    let store = GroupStore::new(home.clone()).map_err(OpError::io)?;
    let ledger_path = store.ledger_path(group_id).map_err(OpError::io)?;
    let wanted = delivery_id(group_id, actor_id, actor_created_at, source_event_id);
    ledger::inspect(&ledger_path, |events, _| {
        events
            .iter()
            .rev()
            .find(|event| {
                event.kind == LAUNCH_EVENT
                    && event.data.get("delivery_id").and_then(Value::as_str)
                        == Some(wanted.as_str())
            })
            .map(|event| event.data.clone())
    })
    .map_err(OpError::io)
}

pub fn latest_state(
    home: &HomeLayout,
    group_id: &str,
    actor_id: &str,
    source_event_id: &str,
) -> Result<Option<(String, String)>, OpError> {
    let store = GroupStore::new(home.clone()).map_err(OpError::io)?;
    let path = store.ledger_path(group_id).map_err(OpError::io)?;
    ledger::inspect(&path, |events, _| {
        events.iter().rev().find_map(|event| {
            (event.kind == "runtime.delivery"
                && event.data.get("actor_id").and_then(Value::as_str) == Some(actor_id)
                && event.data.get("source_event_id").and_then(Value::as_str)
                    == Some(source_event_id))
            .then(|| {
                (
                    event
                        .data
                        .get("state")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    event
                        .data
                        .get("transport")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                )
            })
        })
    })
    .map_err(OpError::io)
}

pub fn claim(
    home: &HomeLayout,
    group: &GroupDoc,
    actor: &Actor,
    source_event_id: &str,
    transport: &str,
    force_ambiguous: bool,
) -> Result<ClaimResult, OpError> {
    let deliveries = [(actor, transport)];
    let (claimed, states) =
        claim_deliveries(home, group, &deliveries, source_event_id, force_ambiguous)?;
    if claimed {
        Ok(ClaimResult::Claimed)
    } else {
        Ok(ClaimResult::Terminal(
            states.get(&actor.id).cloned().unwrap_or_default(),
        ))
    }
}

pub fn claim_deliveries(
    home: &HomeLayout,
    group: &GroupDoc,
    deliveries: &[(&Actor, &str)],
    source_event_id: &str,
    force_ambiguous: bool,
) -> Result<(bool, HashMap<String, String>), OpError> {
    let store = GroupStore::new(home.clone()).map_err(OpError::io)?;
    let ledger_path = store.ledger_path(&group.group_id).map_err(OpError::io)?;
    let lock_path = delivery_lock_path(&ledger_path);
    with_exclusive_lock(&lock_path, || {
        let (claimable, mut states) = ledger::inspect(&ledger_path, |events, _| {
            let mut states = HashMap::new();
            for (actor, _transport) in deliveries {
                let state = events
                    .iter()
                    .rev()
                    .find_map(|event| {
                        (event.kind == "runtime.delivery"
                            && event.data.get("actor_id").and_then(Value::as_str)
                                == Some(actor.id.as_str())
                            && event.data.get("source_event_id").and_then(Value::as_str)
                                == Some(source_event_id))
                        .then(|| {
                            event
                                .data
                                .get("state")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned()
                        })
                    })
                    .unwrap_or_default();
                states.insert(actor.id.clone(), state.clone());
                if state == "claimed" {
                    return (false, states);
                }
                if matches!(state.as_str(), "accepted" | "withdrawn")
                    || (state == "ambiguous" && !force_ambiguous)
                {
                    return (false, states);
                }
            }
            (true, states)
        })?;
        if !claimable {
            return Ok((false, states));
        }
        for (actor, transport) in deliveries {
            append_state(
                home,
                &group.group_id,
                &actor.id,
                &actor.created_at,
                source_event_id,
                transport,
                DeliveryOutcome::Claimed,
            )
            .map_err(|error| std::io::Error::other(error.message))?;
            states.insert(actor.id.clone(), "claimed".into());
        }
        Ok((true, states))
    })
    .map_err(OpError::io)
}

pub fn settle_stranded_claims(home: &HomeLayout, group: &GroupDoc) -> Result<usize, OpError> {
    let store = GroupStore::new(home.clone()).map_err(OpError::io)?;
    let ledger_path = store.ledger_path(&group.group_id).map_err(OpError::io)?;
    with_exclusive_lock(&delivery_lock_path(&ledger_path), || {
        let latest = ledger::inspect(&ledger_path, |events, _| {
            let mut latest = HashMap::<(String, String), (String, String)>::new();
            for event in events {
                if event.kind == "actor.add" {
                    let actor_id = event
                        .data
                        .get("actor")
                        .and_then(Value::as_object)
                        .and_then(|actor| actor.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if !actor_id.is_empty() {
                        latest.retain(|(existing, _), _| existing != actor_id);
                    }
                    continue;
                }
                if event.kind != "runtime.delivery" {
                    continue;
                }
                let actor_id = event
                    .data
                    .get("actor_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let source_event_id = event
                    .data
                    .get("source_event_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if actor_id.is_empty() || source_event_id.is_empty() {
                    continue;
                }
                latest.insert(
                    (actor_id.into(), source_event_id.into()),
                    (
                        event
                            .data
                            .get("state")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .into(),
                        event
                            .data
                            .get("transport")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .into(),
                    ),
                );
            }
            latest
        })?;
        let mut settled = 0;
        for ((actor_id, source_event_id), (state, transport)) in latest {
            let Some(actor) = group.actors.iter().find(|actor| actor.id == actor_id) else {
                continue;
            };
            if state != "claimed" {
                continue;
            }
            append_state(
                home,
                &group.group_id,
                &actor.id,
                &actor.created_at,
                &source_event_id,
                &transport,
                DeliveryOutcome::Ambiguous(
                    "daemon restarted before the claimed handoff recorded an outcome",
                ),
            )
            .map_err(|error| std::io::Error::other(error.message))?;
            settled += 1;
        }
        Ok(settled)
    })
    .map_err(OpError::io)
}

/// Why a stale Mail notice is withdrawn instead of delivered.
pub const STALE_MAIL_NOTICE: &str =
    "Mail notice withdrawn: its Mail was read, answered, or delivered";

/// A Mail notice none of whose Mail still awaits `actor_id`. Every transport
/// checks this right before handing a notice over, retries included. A failed
/// check keeps the notice: a redundant reminder is safer than a lost one.
pub fn is_stale_mail_notice(
    home: &HomeLayout,
    group: &GroupDoc,
    actor_id: &str,
    event: &Event,
) -> bool {
    if event.kind != "system.notify"
        || event.data.get("kind").and_then(Value::as_str) != Some("mail_notice")
    {
        return false;
    }
    let mail = event
        .data
        .get("context")
        .and_then(|context| context.get("source_event_ids"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    cccc_core::mail_pending::still_awaited(home, group, actor_id, &mail)
        .is_ok_and(|awaited| awaited.is_empty())
}

fn delivery_lock_path(ledger_path: &Path) -> PathBuf {
    ledger_path
        .parent()
        .expect("group ledger has a parent")
        .join("state/ledger/runtime_delivery.lock")
}

pub fn pending_sources(
    home: &HomeLayout,
    group: &GroupDoc,
    actor: &Actor,
    limit: usize,
) -> Result<Vec<Event>, OpError> {
    let store = GroupStore::new(home.clone()).map_err(OpError::io)?;
    let path = store.ledger_path(&group.group_id).map_err(OpError::io)?;
    ledger::inspect(&path, |events, _| {
        let generation = events
            .iter()
            .rposition(|event| {
                event.kind == "actor.add"
                    && event
                        .data
                        .get("actor")
                        .and_then(Value::as_object)
                        .and_then(|value| value.get("id"))
                        .and_then(Value::as_str)
                        == Some(actor.id.as_str())
            })
            .map(|index| index + 1)
            .unwrap_or(0);
        let generation_events = &events[generation..];
        let legacy_read_watermark = LegacyReadWatermark::from_events(generation_events, &actor.id);
        let mut latest = Map::<String, Value>::new();
        for event in generation_events {
            if event.kind != "runtime.delivery"
                || event.data.get("actor_id").and_then(Value::as_str) != Some(actor.id.as_str())
            {
                continue;
            }
            if let (Some(source), Some(state)) = (
                event.data.get("source_event_id").and_then(Value::as_str),
                event.data.get("state").and_then(Value::as_str),
            ) {
                latest.insert(source.to_owned(), Value::String(state.to_owned()));
            }
        }
        let mut pending = Vec::new();
        for event in generation_events {
            if event.by == actor.id {
                continue;
            }
            let state = latest
                .get(&event.id)
                .and_then(Value::as_str)
                .unwrap_or_default();
            let addressed = if event.kind == "chat.message" {
                let mode = event
                    .data
                    .get("message_mode")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                (matches!(mode, "send" | "request_reply") || (mode == "mail" && state == "claimed"))
                    && inbox::is_for_actor(group, event, &actor.id)
            } else if event.kind == "system.notify" {
                if legacy_read_watermark.covers_notification(event) {
                    continue;
                }
                let notice_kind = event
                    .data
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if matches!(notice_kind, "mail_notice" | "reply_notice" | "task_notice") {
                    event.data.get("target_actor_id").and_then(Value::as_str)
                        == Some(actor.id.as_str())
                } else {
                    (notice_kind == "memory_restart"
                        && event.data.get("target_actor_id").and_then(Value::as_str)
                            == Some(actor.id.as_str()))
                        || inbox::is_for_actor(group, event, &actor.id)
                }
            } else {
                false
            };
            if !addressed {
                continue;
            }
            if !matches!(state, "accepted" | "ambiguous" | "withdrawn") {
                pending.push(event.clone());
                if pending.len() >= limit.max(1) {
                    break;
                }
            }
        }
        pending
    })
    .map_err(OpError::io)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    #[test]
    fn concurrent_claims_are_unique_and_restart_settles_the_stranded_claim() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let mut group = store.create("runtime delivery", "").expect("group");
        let actor = Actor::new("peer1");
        group.actors.push(actor.clone());
        store.save(&group).expect("save actor");

        let barrier = Arc::new(Barrier::new(2));
        let results = std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for _ in 0..2 {
                let barrier = barrier.clone();
                let home = &home;
                let group = &group;
                let actor = &actor;
                handles.push(scope.spawn(move || {
                    barrier.wait();
                    claim(home, group, actor, "source-1", "pty", false).expect("claim")
                }));
            }
            handles
                .into_iter()
                .map(|handle| handle.join().expect("claim thread"))
                .collect::<Vec<_>>()
        });
        assert_eq!(
            results
                .iter()
                .filter(|result| **result == ClaimResult::Claimed)
                .count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|result| **result == ClaimResult::Terminal("claimed".into()))
                .count(),
            1
        );

        let ledger_path = store.ledger_path(&group.group_id).expect("ledger path");
        let states = ledger::read_all(&ledger_path)
            .expect("ledger")
            .into_iter()
            .filter(|event| event.kind == "runtime.delivery")
            .map(|event| {
                event
                    .data
                    .get("state")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect::<Vec<_>>();
        assert_eq!(states, ["claimed"]);

        assert_eq!(settle_stranded_claims(&home, &group).expect("settle"), 1);
        assert_eq!(
            latest_state(&home, &group.group_id, &actor.id, "source-1")
                .expect("latest")
                .expect("state")
                .0,
            "ambiguous"
        );
        assert_eq!(
            claim(&home, &group, &actor, "source-1", "pty", false).expect("blocked retry"),
            ClaimResult::Terminal("ambiguous".into())
        );
        assert_eq!(
            claim(&home, &group, &actor, "source-1", "pty", true).expect("forced retry"),
            ClaimResult::Claimed
        );
    }

    #[test]
    fn pending_sources_apply_legacy_read_watermark_to_notification_prefix() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let mut group = store.create("legacy read recovery", "").expect("group");
        let actor = Actor::new("peer1");
        group.actors.push(actor.clone());
        store.save(&group).expect("save actor");
        let ledger_path = store.ledger_path(&group.group_id).expect("ledger");

        let mut old_message = Event::new("chat.message", &group.group_id);
        old_message.by = "user".into();
        old_message.data = json!({"to":["peer1"],"text":"already read"})
            .as_object()
            .cloned()
            .expect("message data");
        ledger::append(&ledger_path, &old_message).expect("old message");
        let mut first_notice = Event::new("system.notify", &group.group_id);
        first_notice.by = "system".into();
        first_notice.data = json!({
            "to":["peer1"],
            "kind":"info",
            "context":{"event_id":old_message.id}
        })
        .as_object()
        .cloned()
        .expect("notice data");
        ledger::append(&ledger_path, &first_notice).expect("first notice");
        let mut watermark_notice = Event::new("system.notify", &group.group_id);
        watermark_notice.by = "system".into();
        watermark_notice.data = json!({"to":["peer1"],"kind":"unread_nudge"})
            .as_object()
            .cloned()
            .expect("notice data");
        ledger::append(&ledger_path, &watermark_notice).expect("watermark notice");
        let mut read = Event::new("chat.read", &group.group_id);
        read.by = actor.id.clone();
        read.data = json!({"actor_id":actor.id,"event_id":watermark_notice.id})
            .as_object()
            .cloned()
            .expect("read data");
        ledger::append(&ledger_path, &read).expect("legacy read watermark");
        let mut late_linked_notice = Event::new("system.notify", &group.group_id);
        late_linked_notice.by = "system".into();
        late_linked_notice.data = json!({
            "to":["peer1"],
            "kind":"unread_nudge",
            "context":{"event_id":old_message.id}
        })
        .as_object()
        .cloned()
        .expect("late linked notice data");
        ledger::append(&ledger_path, &late_linked_notice).expect("late linked notice");
        let mut current_notice = Event::new("system.notify", &group.group_id);
        current_notice.by = "system".into();
        current_notice.data = json!({"to":["peer1"],"kind":"info"})
            .as_object()
            .cloned()
            .expect("current notice data");
        ledger::append(&ledger_path, &current_notice).expect("current notice");
        let mut current = Event::new("chat.message", &group.group_id);
        current.by = "user".into();
        current.data = json!({"to":["peer1"],"text":"current","message_mode":"send"})
            .as_object()
            .cloned()
            .expect("current data");
        ledger::append(&ledger_path, &current).expect("current message");

        let pending = pending_sources(&home, &group, &actor, 10).expect("pending sources");

        assert_eq!(
            pending
                .iter()
                .map(|event| event.id.as_str())
                .collect::<Vec<_>>(),
            [current_notice.id.as_str(), current.id.as_str()]
        );
    }

    #[test]
    fn the_handover_is_recorded_while_the_delivery_is_still_only_claimed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let mut group = store.create("handover ordering", "").expect("group");
        let actor = Actor::new("peer1");
        group.actors.push(actor.clone());
        store.save(&group).expect("save actor");

        assert_eq!(
            claim(&home, &group, &actor, "source-1", "pty", false).expect("claim"),
            ClaimResult::Claimed
        );
        assert!(
            launch_record(&home, &group.group_id, &actor.id, &actor.created_at, "source-1")
                .expect("launch record")
                .is_none(),
            "a claimed delivery has not been handed to a process yet"
        );

        let handover = LaunchIdentity::new(
            &group.group_id,
            &actor.id,
            Some(4242),
            "2026-10-20T10:00:00Z",
            "pty",
        );
        record_launch(
            &home,
            &group.group_id,
            &actor.id,
            &actor.created_at,
            "source-1",
            "pty",
            &handover,
        )
        .expect("record handover");

        // The record exists before the terminal accepts anything: the delivery
        // is still only claimed, so an incident replay cannot wait for "accepted".
        assert_eq!(
            latest_state(&home, &group.group_id, &actor.id, "source-1")
                .expect("latest")
                .expect("state")
                .0,
            "claimed"
        );
        let record = launch_record(
            &home,
            &group.group_id,
            &actor.id,
            &actor.created_at,
            "source-1",
        )
        .expect("launch record")
        .expect("recorded");
        assert_eq!(record.get("pid").and_then(Value::as_u64), Some(4242));
        assert_eq!(
            record.get("started_at").and_then(Value::as_str),
            Some("2026-10-20T10:00:00Z")
        );
        assert_eq!(
            record.get("session_key").and_then(Value::as_str),
            Some(format!("{}:peer1", group.group_id).as_str())
        );
        assert_eq!(record.get("transport").and_then(Value::as_str), Some("pty"));
    }

    #[test]
    fn one_line_per_process_and_the_record_survives_a_daemon_restart() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let mut group = store.create("handover restart", "").expect("group");
        let actor = Actor::new("peer1");
        group.actors.push(actor.clone());
        store.save(&group).expect("save actor");
        let ledger_path = store.ledger_path(&group.group_id).expect("ledger");
        assert_eq!(
            claim(&home, &group, &actor, "source-1", "pty", false).expect("claim"),
            ClaimResult::Claimed
        );

        let first =
            LaunchIdentity::new(&group.group_id, &actor.id, Some(111), "2026-10-20T10:00:00Z", "pty");
        for _ in 0..2 {
            record_launch(
                &home,
                &group.group_id,
                &actor.id,
                &actor.created_at,
                "source-1",
                "pty",
                &first,
            )
            .expect("record handover");
        }
        let launches = |path: &std::path::Path| {
            ledger::read_all(path)
                .expect("ledger")
                .into_iter()
                .filter(|event| event.kind == LAUNCH_EVENT)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            launches(&ledger_path).len(),
            1,
            "a retry onto the same process must not duplicate the line"
        );

        // A relaunch is its own handover and stays visible too.
        let second =
            LaunchIdentity::new(&group.group_id, &actor.id, Some(222), "2026-10-20T10:05:00Z", "pty");
        record_launch(
            &home,
            &group.group_id,
            &actor.id,
            &actor.created_at,
            "source-1",
            "pty",
            &second,
        )
        .expect("record relaunch");
        assert_eq!(launches(&ledger_path).len(), 2);

        // Restart: a fresh home handle replays the same ledger, and the claim the
        // restart settles does not drop the handover it belonged to.
        let reopened = HomeLayout::from_path(temp.path().join("home")).expect("home");
        settle_stranded_claims(&reopened, &group).expect("settle");
        let record = launch_record(
            &reopened,
            &group.group_id,
            &actor.id,
            &actor.created_at,
            "source-1",
        )
        .expect("launch record")
        .expect("recorded");
        assert_eq!(record.get("pid").and_then(Value::as_u64), Some(222));
        assert_eq!(
            latest_state(&reopened, &group.group_id, &actor.id, "source-1")
                .expect("latest")
                .expect("state")
                .0,
            "ambiguous"
        );
    }

    #[test]
    fn batch_claim_rejects_a_conflict_before_claiming_other_recipients() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let mut group = store.create("runtime delivery batch", "").expect("group");
        let first = Actor::new("peer1");
        let second = Actor::new("peer2");
        group.actors.extend([first.clone(), second.clone()]);
        store.save(&group).expect("save actors");

        assert_eq!(
            claim(&home, &group, &second, "source-1", "pty", false).expect("reserve second"),
            ClaimResult::Claimed
        );
        let requests = [(&first, "pty"), (&second, "pty")];
        let (claimed, states) =
            claim_deliveries(&home, &group, &requests, "source-1", false).expect("batch claim");

        assert!(!claimed);
        assert_eq!(states.get("peer2").map(String::as_str), Some("claimed"));
        assert_eq!(
            latest_state(&home, &group.group_id, "peer1", "source-1").expect("first state"),
            None
        );
    }
}
