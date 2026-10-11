use super::{Session, block_on_managed, supervisor};
use cccc_contracts::{Actor, ActorRuntime, Event};
use serde_json::json;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

pub(crate) fn enabled(actor: &Actor) -> bool {
    matches!(actor.runtime, ActorRuntime::Kilo | ActorRuntime::Opencode)
        && actor
            .env
            .get("CCCC_OPENCODE_HEADLESS")
            .is_some_and(|value| value == "1")
}

/// Owns a terminal connection, not a provider session. The last connection
/// closes only the lazy TUI; a stale connection cannot close a replacement.
pub(crate) struct ViewerLease {
    session: Weak<Session>,
}

pub(crate) fn acquire(group_id: &str, actor_id: &str) -> io::Result<Option<ViewerLease>> {
    let key = (group_id.to_owned(), actor_id.to_owned());
    let _guard = supervisor::StartGuard::acquire(&key)?;
    let Some(item) = supervisor::lookup(&key).filter(|item| item.lazy_viewer) else {
        return Ok(None);
    };
    item.reattach_viewer()?;
    item.viewer_clients.fetch_add(1, Ordering::AcqRel);
    Ok(Some(ViewerLease {
        session: Arc::downgrade(&item),
    }))
}

impl Drop for ViewerLease {
    fn drop(&mut self) {
        let Some(item) = self.session.upgrade() else {
            return;
        };
        let key = (item.group_id.clone(), item.actor_id.clone());
        let Ok(_guard) = supervisor::StartGuard::acquire(&key) else {
            return;
        };
        if !supervisor::lookup(&key).is_some_and(|current| Arc::ptr_eq(&current, &item)) {
            return;
        }
        if item.viewer_clients.fetch_sub(1, Ordering::AcqRel) == 1
            && let Err(error) = item.close_viewer()
        {
            tracing::warn!(%error, group_id = %item.group_id, actor_id = %item.actor_id,
                "failed to close unused managed viewer");
        }
    }
}

pub(super) fn submit(
    item: &Session,
    events: &[Event],
    delivery: &str,
    cancelled: &AtomicBool,
) -> bool {
    // A worker keeps the original event batch on retry. Its first source ID
    // is stable, so an accepted prompt is never submitted again after an ack
    // failure. ACP independently refuses admission while a turn is active.
    let Some(event) = events.first() else {
        return false;
    };
    if cancelled.load(Ordering::Acquire) {
        return false;
    }
    let Ok(mut startup) = item.startup_prompt.lock() else {
        return false;
    };
    let prepared = startup.as_ref().map_or_else(
        || delivery.to_owned(),
        |prompt| format!("{prompt}\n\n{delivery}"),
    );
    match block_on_managed(item.managed.start_delivery(&event.id, &prepared)) {
        Ok(_) => {
            *startup = None;
            true
        }
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => false,
        Err(error) => {
            let data = json!({"pending_event_id":event.id,"turn_id":event.id,
                "error":{"message":format!("ACP delivery {} failed: {}", event.id,
                    super::super::codex_voice_analyst::AnalystSession::delivery_error_message(&error))},
                "retryable":super::super::codex_voice_analyst::AnalystSession::delivery_retryable(&error)})
            .as_object()
            .expect("object")
            .clone();
            if let Err(emit_error) = super::events::append_with_dedupe(
                &item.home,
                &item.group_id,
                &item.actor_id,
                "headless.turn.failed",
                data,
                Some(&format!("acp-delivery-failed:{}", event.id)),
            ) {
                tracing::warn!(%emit_error, "failed to publish managed delivery error");
            }
            false
        }
    }
}

/// Only an actual interrupt byte is diverted; arbitrary text keeps the native
/// terminal protocol. The web Stop button sends Ctrl-C through this same path.
pub(crate) fn interrupt(group_id: &str, actor_id: &str, data: &[u8]) -> io::Result<bool> {
    if data != b"\x03" {
        return Ok(false);
    }
    let Some(item) = supervisor::lookup(&(group_id.to_owned(), actor_id.to_owned()))
        .filter(|item| item.lazy_viewer)
    else {
        return Ok(false);
    };
    let turn = item
        .active_turn
        .lock()
        .map_err(|_| super::poisoned())?
        .as_ref()
        .map(|turn| turn.turn_id.clone());
    if let Some(turn_id) = turn {
        block_on_managed(item.managed.interrupt(item.managed.generation(), &turn_id))?;
    }
    Ok(true)
}

/// Retire only after the existing delivery ledger durably records admission.
pub(crate) fn delivery_accepted(group_id: &str, actor_id: &str, event_id: &str) {
    if let Some(item) = supervisor::lookup(&(group_id.to_owned(), actor_id.to_owned()))
        .filter(|item| item.lazy_viewer)
    {
        block_on_managed(item.managed.forget_delivery(event_id));
    }
}

/// An uncertain admission is terminal for this source, not a retry that blocks
/// every later message forever. Its durable ambiguous record prevents replay.
pub(crate) fn unresolved_delivery(
    group_id: &str,
    actor_id: &str,
    event_id: &str,
) -> Option<String> {
    let item = supervisor::lookup(&(group_id.to_owned(), actor_id.to_owned()))
        .filter(|item| item.lazy_viewer)?;
    block_on_managed(item.managed.unresolved_delivery(event_id))
}
