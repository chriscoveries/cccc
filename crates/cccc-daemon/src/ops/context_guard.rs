//! Cap on repeated oversized context answers (owner spec dd17cbba, item 3).
//!
//! CCCC has no retry loop of its own: the loop that corrupted DSH sessions was
//! a caller re-asking for an answer it could not carry. What the daemon can
//! enforce is the other half of item 3 — after a bounded number of oversized
//! answers to the same actor, stop serving them and surface one bounded error
//! with a retry delay, so a caller that retries without paging is refused
//! instead of being fed another multi-megabyte projection.
//!
//! The count is per (group, actor) and short-lived: three answers over the
//! state budget inside ten seconds trip it, a bounded (paged) request from the
//! same actor clears it, and the cooldown then passes page-sized answers only.
//! Bootstrap asks for the unbounded snapshot once per session, which stays
//! under the cap; a retry loop does not.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::dispatch::OpError;

pub(super) const MAX_OVERSIZE_ANSWERS: usize = 3;
pub(super) const ATTEMPT_WINDOW: Duration = Duration::from_secs(10);
pub(super) const COOLDOWN: Duration = Duration::from_secs(30);

#[derive(Debug, Default, Clone, Copy)]
struct Entry {
    attempts: usize,
    first: Option<Instant>,
    refused_until: Option<Instant>,
}

fn registry() -> &'static Mutex<HashMap<String, Entry>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, Entry>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn key(group_id: &str, actor_id: &str) -> String {
    format!("{group_id}\u{1f}{actor_id}")
}

/// `Some` when this actor must be refused instead of served.
///
/// An answer without a caller identity is not counted and not refused: the cap
/// protects a session, and a request that names no session has none to protect.
pub(super) fn refusal(group_id: &str, actor_id: &str, now: Instant) -> Option<OpError> {
    if actor_id.is_empty() {
        return None;
    }
    let Ok(map) = registry().lock() else {
        return None;
    };
    let entry = map.get(&key(group_id, actor_id))?;
    let until = entry.refused_until?;
    if until <= now {
        return None;
    }
    let wait = until.saturating_duration_since(now);
    let mut error = OpError::new(
        "context_too_large",
        format!(
            "context_get refused for {actor_id}: {MAX_OVERSIZE_ANSWERS} unbounded answers in {}s \
             exceeded the state budget; ask with limit/cursor (or summary=true) and retry in {}s",
            ATTEMPT_WINDOW.as_secs(),
            wait.as_secs().max(1)
        ),
    );
    error.details.insert(
        "retry_after_ms".into(),
        serde_json::json!(wait.as_millis() as u64),
    );
    error
        .details
        .insert("attempts".into(), serde_json::json!(entry.attempts));
    error.details.insert(
        "page_size_default".into(),
        serde_json::json!(super::context_bounds::DEFAULT_LIMIT),
    );
    Some(error)
}

/// Record one served answer. An answer over `max_bytes` counts towards the cap;
/// one within it clears the record, because that caller is paging.
pub(super) fn record(group_id: &str, actor_id: &str, bytes: usize, max_bytes: usize, now: Instant) {
    if actor_id.is_empty() {
        return;
    }
    let Ok(mut map) = registry().lock() else {
        return;
    };
    let entry = map.entry(key(group_id, actor_id)).or_default();
    if bytes <= max_bytes {
        *entry = Entry::default();
        return;
    }
    let expired = match entry.first {
        Some(first) => now.saturating_duration_since(first) > ATTEMPT_WINDOW,
        None => true,
    };
    if expired {
        *entry = Entry {
            attempts: 1,
            first: Some(now),
            refused_until: None,
        };
        return;
    }
    entry.attempts += 1;
    if entry.attempts >= MAX_OVERSIZE_ANSWERS {
        entry.refused_until = Some(now + COOLDOWN);
    }
}

/// Forget an actor's record (used by paged answers and by tests).
pub(super) fn clear(group_id: &str, actor_id: &str) {
    if actor_id.is_empty() {
        return;
    }
    if let Ok(mut map) = registry().lock() {
        map.remove(&key(group_id, actor_id));
    }
}

/// Empty the registry. Test-only: the daemon keeps one process-wide registry.
#[cfg(test)]
pub(super) fn reset() {
    if let Ok(mut map) = registry().lock() {
        map.clear();
    }
}

#[cfg(test)]
#[path = "context_guard_tests.rs"]
mod tests;
