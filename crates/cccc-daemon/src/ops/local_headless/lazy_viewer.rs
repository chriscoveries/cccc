//! Connection-owned native viewers; the managed provider outlives every lease.
use super::{Session, block_on_managed, supervisor};
use cccc_contracts::{Actor, ActorRuntime, Event};
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};

#[cfg(all(test, target_os = "linux"))]
mod live_tests;

pub(super) struct LazyViewer {
    clients: AtomicUsize,
    actor_created_at: String,
    pending: Mutex<BTreeMap<String, BTreeSet<String>>>,
}

impl LazyViewer {
    pub(super) fn for_actor(actor: &Actor) -> Option<Self> {
        (matches!(actor.runtime, ActorRuntime::Kilo | ActorRuntime::Opencode)
            && actor
                .env
                .get("CCCC_OPENCODE_HEADLESS")
                .is_some_and(|v| v == "1"))
        .then(|| Self {
            clients: AtomicUsize::new(0),
            actor_created_at: actor.created_at.clone(),
            pending: Mutex::new(BTreeMap::new()),
        })
    }
}

pub(crate) struct ViewerLease(Weak<Session>);

impl ViewerLease {
    pub(crate) fn interrupt_generation(&self) -> Option<String> {
        self.0
            .upgrade()
            // OpenCode V2 deliberately refuses exact-turn HTTP cancellation.
            // Its native TUI keys keep their existing behavior.
            .filter(|item| item.managed.runtime() == ActorRuntime::Kilo)
            .map(|item| item.managed.generation().to_owned())
    }
}

pub(crate) fn acquire(group_id: &str, actor_id: &str) -> io::Result<Option<ViewerLease>> {
    let key = (group_id.to_owned(), actor_id.to_owned());
    let _guard = supervisor::StartGuard::acquire(&key)?;
    let Some(item) = supervisor::lookup(&key).filter(|item| item.lazy_viewer.is_some()) else {
        return Ok(None);
    };
    item.reattach_viewer()?;
    item.lazy_viewer
        .as_ref()
        .expect("filtered lazy viewer")
        .clients
        .fetch_add(1, Ordering::AcqRel);
    Ok(Some(ViewerLease(Arc::downgrade(&item))))
}

impl Drop for ViewerLease {
    fn drop(&mut self) {
        let owner = self.0.clone();
        // Native terminal teardown and start serialization may wait. Never do
        // that work on the async terminal connection's executor thread.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn_blocking(move || release(owner));
        } else {
            release(owner);
        }
    }
}

fn release(owner: Weak<Session>) {
    let Some(item) = owner.upgrade() else { return };
    let key = (item.group_id.clone(), item.actor_id.clone());
    let Ok(_guard) = supervisor::StartGuard::acquire(&key) else {
        return;
    };
    if !supervisor::lookup(&key).is_some_and(|current| Arc::ptr_eq(&current, &item)) {
        return;
    }
    if item
        .lazy_viewer
        .as_ref()
        .expect("leased lazy viewer")
        .clients
        .fetch_sub(1, Ordering::AcqRel)
        == 1
        && let Err(error) = item.close_viewer()
    {
        tracing::warn!(%error, "failed to close unused managed viewer");
    }
}

/// Called only after a terminal delivery outcome has been durably appended.
/// A failed append retains its admission receipt until the completion retry.
pub(crate) fn settled_delivery(group_id: &str, actor_id: &str, created_at: &str, source: &str) {
    if let Some(item) = supervisor::lookup(&(group_id.to_owned(), actor_id.to_owned()))
        && item
            .lazy_viewer
            .as_ref()
            .is_some_and(|viewer| viewer.actor_created_at == created_at)
    {
        let completed = {
            let Ok(mut pending) = item
                .lazy_viewer
                .as_ref()
                .expect("filtered lazy viewer")
                .pending
                .lock()
            else {
                return;
            };
            let mut completed = Vec::new();
            pending.retain(|id, sources| {
                sources.remove(source);
                if sources.is_empty() {
                    completed.push(id.clone());
                    false
                } else {
                    true
                }
            });
            completed
        };
        for id in completed {
            block_on_managed(item.managed.forget_delivery(&id));
        }
    }
}

pub(super) async fn submit(
    item: &Session,
    id: &str,
    events: &[Event],
    text: &str,
) -> io::Result<super::super::codex_voice_analyst::TurnReceipt> {
    {
        let mut pending = item
            .lazy_viewer
            .as_ref()
            .expect("lazy submission")
            .pending
            .lock()
            .map_err(|_| super::poisoned())?;
        if !pending.contains_key(id) && pending.len() >= 256 {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "pending delivery settlements are full",
            ));
        }
        pending
            .entry(id.to_owned())
            .or_insert_with(|| events.iter().map(|event| event.id.clone()).collect());
    }
    let result = item.managed.start_delivery(id, text).await;
    if result
        .as_ref()
        .is_err_and(|error| error.kind() == io::ErrorKind::WouldBlock)
    {
        item.lazy_viewer
            .as_ref()
            .expect("lazy submission")
            .pending
            .lock()
            .map_err(|_| super::poisoned())?
            .remove(id);
    }
    result
}

/// The caller must hold terminal writer authority across this control call.
pub(crate) fn interrupt(group_id: &str, actor_id: &str, generation: &str) -> io::Result<()> {
    let Some(item) =
        supervisor::lookup(&(group_id.to_owned(), actor_id.to_owned())).filter(|item| {
            item.lazy_viewer.is_some()
                && item.managed.runtime() == ActorRuntime::Kilo
                && item.managed.generation() == generation
        })
    else {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "managed viewer owner was replaced",
        ));
    };
    let turn = item
        .active_turn
        .lock()
        .map_err(|_| super::poisoned())?
        .as_ref()
        .map(|turn| turn.turn_id.clone());
    if let Some(turn) = turn {
        block_on_managed(item.managed.interrupt(item.managed.generation(), &turn))?;
    }
    Ok(())
}
