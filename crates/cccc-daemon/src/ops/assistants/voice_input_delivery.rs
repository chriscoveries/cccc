use cccc_contracts::Event;
use cccc_core::{GroupStore, HomeLayout, ledger};
use serde_json::{Value, json};

use crate::dispatch::OpError;
use crate::ops::{actor_delivery, actor_runtime};

const ACTOR_ID: &str = "voice-secretary";

#[derive(Default)]
pub(super) struct DeliveryOutcome {
    pub(super) event: Option<Event>,
    pub(super) notify: Option<Event>,
    pub(super) delivery: Option<Value>,
    pub(super) actor_woken: bool,
    pub(super) wake_error: String,
}

pub(super) fn deliver(
    home: &HomeLayout,
    store: &GroupStore,
    group_id: &str,
    session_id: &str,
    segment_id: &str,
    by: &str,
    candidate_input: Option<&Value>,
) -> Result<DeliveryOutcome, OpError> {
    let Some(input) = candidate_input else {
        return Ok(DeliveryOutcome::default());
    };
    let group = store.load(group_id).map_err(OpError::not_found)?;
    let needs_notice = group.actors.iter().any(|actor| actor.id == ACTOR_ID);
    let (prior_input, prior_notice) = events_for_segment(store, group_id, session_id, segment_id)?;
    if prior_input.is_some() && (!needs_notice || prior_notice.is_some()) {
        return Ok(DeliveryOutcome::default());
    }

    let ledger_path = store.ledger_path(group_id).map_err(OpError::io)?;
    let input_event = if let Some(event) = prior_input {
        event
    } else {
        let mut event = Event::new("assistant.voice.input", group_id);
        event.by = by.into();
        event.data = input.as_object().cloned().unwrap_or_default();
        ledger::append(&ledger_path, &event).map_err(OpError::io)?;
        event
    };
    let mut outcome = DeliveryOutcome {
        event: Some(input_event),
        ..DeliveryOutcome::default()
    };

    if !needs_notice {
        return Ok(outcome);
    }
    // The notification's delivery worker owns automatic startup, including its cancellable
    // restart backoff. Starting here would bypass that wait for every voice segment.
    outcome.actor_woken = group
        .actors
        .iter()
        .find(|actor| actor.id == ACTOR_ID)
        .is_some_and(|actor| actor_runtime::actor_is_running(&group, actor));
    let notice = if let Some(event) = prior_notice {
        event
    } else {
        let mut event = Event::new("system.notify", group_id);
        event.by = "system".into();
        event.data = json!({
            "kind":"voice_secretary_input",
            "title":"Voice Secretary input",
            "text":"New voice input is ready.",
            "to":[ACTOR_ID],
            "priority":"normal",
            "context":{"kind":"voice_secretary_input","input_envelope":input}
        })
        .as_object()
        .cloned()
        .unwrap_or_default();
        ledger::append(&ledger_path, &event).map_err(OpError::io)?;
        event
    };
    outcome.delivery = serde_json::to_value(actor_delivery::dispatch(home, &group, &notice)).ok();
    outcome.notify = Some(notice);
    Ok(outcome)
}

fn events_for_segment(
    store: &GroupStore,
    group_id: &str,
    session_id: &str,
    segment_id: &str,
) -> Result<(Option<Event>, Option<Event>), OpError> {
    let events = ledger::read_all(&store.ledger_path(group_id).map_err(OpError::io)?)
        .map_err(OpError::io)?;
    let input = events
        .iter()
        .find(|event| {
            event.kind == "assistant.voice.input"
                && event_data_string(event, &["session_id"]) == Some(session_id)
                && event_data_string(event, &["segment_id"]) == Some(segment_id)
        })
        .cloned();
    let notice = events
        .iter()
        .find(|event| {
            event.kind == "system.notify"
                && event_data_string(event, &["kind"]) == Some("voice_secretary_input")
                && event_data_string(event, &["context", "input_envelope", "session_id"])
                    == Some(session_id)
                && event_data_string(event, &["context", "input_envelope", "segment_id"])
                    == Some(segment_id)
        })
        .cloned();
    Ok((input, notice))
}

fn event_data_string<'a>(event: &'a Event, path: &[&str]) -> Option<&'a str> {
    let (first, rest) = path.split_first()?;
    let mut value = event.data.get(*first)?;
    for key in rest {
        value = value.get(*key)?;
    }
    value.as_str()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use cccc_contracts::{Actor, ActorRuntime, GroupState};
    use cccc_core::Scope;
    use std::time::Duration;

    #[test]
    fn voice_input_wake_uses_the_cancellable_delivery_backoff() {
        let temp = tempfile::tempdir().expect("fixture");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let mut group = store.create("voice restart", "").expect("group");
        group.running = true;
        group.state = GroupState::Active;
        group.scopes.push(Scope {
            scope_key: "project".into(),
            url: temp.path().to_string_lossy().into_owned(),
            label: "project".into(),
            git_remote: String::new(),
        });
        group.active_scope_key = "project".into();
        let launched = temp.path().join("launched");
        let mut actor = Actor::new(ACTOR_ID);
        actor.runtime = ActorRuntime::Custom;
        actor.command = vec![
            "sh".into(),
            "-c".into(),
            "touch \"$1\"; exit 1".into(),
            "fixture".into(),
            launched.to_string_lossy().into_owned(),
        ];
        group.actors.push(actor.clone());
        store.save(&group).expect("save");
        use crate::ops::actor_respawn_backoff as backoff;
        assert!(backoff::begin_restart(&group.group_id, ACTOR_ID).is_zero());
        backoff::end_restart(&group.group_id, ACTOR_ID);
        let outcome = deliver(
            &home,
            &store,
            &group.group_id,
            "session",
            "segment",
            "user",
            Some(&json!({"session_id":"session","segment_id":"segment"})),
        )
        .expect("voice delivery");
        std::thread::sleep(Duration::from_millis(250));
        let stopped = std::time::Instant::now();
        actor_delivery::shutdown_actor(&group.group_id, ACTOR_ID);
        let _ = cccc_runtime::stop(&group.group_id, ACTOR_ID);
        backoff::forget(&group.group_id, ACTOR_ID);
        assert!(
            stopped.elapsed() < Duration::from_secs(1),
            "shutdown must interrupt the wait"
        );
        assert!(!outcome.actor_woken, "a queued wake has not launched yet");
        assert_eq!(outcome.delivery.expect("dispatch report")["queued"], 1);
        assert!(
            !launched.exists(),
            "voice input must not launch around the worker's backoff"
        );
    }
}
