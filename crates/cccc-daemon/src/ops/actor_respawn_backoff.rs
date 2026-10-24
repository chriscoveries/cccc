//! Extra pacing for repeated successful automatic starts followed by exit.
//! Failed launches retain delivery's ordinary retry pacing, without this penalty.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const WINDOW: Duration = Duration::from_secs(60);
type Key = (String, String);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Restarts {
    count: u32,
    last: Instant,
}

#[derive(Default)]
struct Entry {
    committed: Option<Restarts>,
    pending: Option<Arc<()>>,
}

fn history() -> &'static Mutex<HashMap<Key, Entry>> {
    static HISTORY: OnceLock<Mutex<HashMap<Key, Entry>>> = OnceLock::new();
    HISTORY.get_or_init(Default::default)
}

fn plan(previous: Option<Restarts>, now: Instant) -> (Duration, u32) {
    let earlier = previous
        .filter(|state| now.saturating_duration_since(state.last) < WINDOW)
        .map_or(0, |state| state.count);
    let delay = if earlier == 0 {
        Duration::ZERO
    } else {
        Duration::from_secs(10 * (1_u64 << earlier.saturating_sub(1).min(3))).min(WINDOW)
    };
    (delay, earlier.saturating_add(1))
}

/// A reservation is not restart history. Drop abandons every unlaunched path.
pub(super) struct Attempt {
    key: Key,
    reservation: Arc<()>,
    count: u32,
    pub(super) delay: Duration,
}

pub(super) fn begin(group: &str, actor: &str) -> Option<Attempt> {
    begin_at(group, actor, Instant::now())
}

fn begin_at(group: &str, actor: &str, now: Instant) -> Option<Attempt> {
    let key = (group.to_owned(), actor.to_owned());
    let mut map = history().lock().ok()?;
    let entry = map.entry(key.clone()).or_default();
    if entry.pending.is_some() {
        return None;
    }
    let (delay, count) = plan(entry.committed, now);
    entry.committed = Some(Restarts {
        count,
        last: now + delay,
    });
    let reservation = Arc::new(());
    entry.pending = Some(Arc::clone(&reservation));
    Some(Attempt {
        key,
        reservation,
        count,
        delay,
    })
}

impl Attempt {
    pub(super) fn complete(self, came_up: bool) {
        self.complete_at(came_up, Instant::now());
    }

    fn complete_at(self, _came_up: bool, now: Instant) {
        if let Ok(mut map) = history().lock()
            && let Some(entry) = map.get_mut(&self.key)
            && entry
                .pending
                .as_ref()
                .is_some_and(|token| Arc::ptr_eq(token, &self.reservation))
        {
            entry.committed = Some(Restarts {
                count: self.count,
                last: now,
            });
        }
    }
}

impl Drop for Attempt {
    fn drop(&mut self) {
        if let Ok(mut map) = history().lock()
            && let Some(entry) = map.get_mut(&self.key)
            && entry
                .pending
                .as_ref()
                .is_some_and(|token| Arc::ptr_eq(token, &self.reservation))
        {
            entry.pending = None;
            if entry.committed.is_none() {
                map.remove(&self.key);
            }
        }
    }
}

/// Removing the reservation also fences a late completion, even without history.
pub(super) fn forget(group: &str, actor: &str) {
    if let Ok(mut map) = history().lock() {
        map.remove(&(group.to_owned(), actor.to_owned()));
    }
}

#[cfg(test)]
pub(super) fn pending(group: &str, actor: &str) -> bool {
    history()
        .lock()
        .expect("history")
        .get(&(group.into(), actor.into()))
        .is_some_and(|entry| entry.pending.is_some())
}

#[cfg(test)]
pub(super) fn observed(group: &str, actor: &str) -> (Duration, Option<(u32, Instant)>) {
    let state = history()
        .lock()
        .expect("history")
        .get(&(group.into(), actor.into()))
        .and_then(|entry| entry.committed);
    (
        plan(state, Instant::now()).0,
        state.map(|state| (state.count, state.last)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_preserved_schedule_doubles_caps_and_resets_after_a_quiet_window() {
        let now = Instant::now();
        let delays: Vec<_> = (0..6)
            .map(|count| plan(Some(Restarts { count, last: now }), now).0.as_secs())
            .collect();
        assert_eq!(delays, [0, 10, 20, 40, 60, 60]);
        assert_eq!(
            plan(
                Some(Restarts {
                    count: u32::MAX,
                    last: now
                }),
                now
            )
            .0,
            WINDOW
        );
        assert_eq!(
            plan(
                Some(Restarts {
                    count: 4,
                    last: now
                }),
                now + WINDOW
            ),
            (Duration::ZERO, 1)
        );
    }

    #[test]
    fn failure_or_cancellation_preserves_committed_count_and_timestamp() {
        let group = uuid::Uuid::new_v4().to_string();
        let now = Instant::now();
        begin_at(&group, "peer", now)
            .expect("first")
            .complete_at(true, now);
        let before = observed(&group, "peer");
        let attempt = begin_at(&group, "peer", now + Duration::from_secs(1)).expect("retry");
        assert_eq!(attempt.delay, Duration::from_secs(10));
        attempt.complete_at(false, now + Duration::from_secs(11));
        assert_eq!(observed(&group, "peer"), before);
        drop(begin_at(&group, "peer", now + Duration::from_secs(12)).expect("cancelled"));
        assert_eq!(observed(&group, "peer"), before);
        forget(&group, "peer");
    }

    #[test]
    fn a_capped_wait_keeps_the_loop_decided_before_the_wait() {
        let group = uuid::Uuid::new_v4().to_string();
        let now = Instant::now();
        history().lock().expect("history").insert(
            (group.clone(), "peer".into()),
            Entry {
                committed: Some(Restarts {
                    count: 4,
                    last: now,
                }),
                pending: None,
            },
        );
        let attempt = begin_at(&group, "peer", now).expect("capped");
        assert_eq!(attempt.delay, WINDOW);
        attempt.complete_at(true, now + WINDOW + Duration::from_secs(2));
        let state = history()
            .lock()
            .expect("history")
            .get(&(group.clone(), "peer".into()))
            .and_then(|entry| entry.committed)
            .expect("committed");
        assert_eq!(state.count, 5);
        assert_eq!(
            plan(Some(state), state.last + Duration::from_secs(5)).0,
            WINDOW
        );
        forget(&group, "peer");
    }

    #[test]
    fn forget_invalidates_pending_attempts_without_tombstones_or_aba() {
        for initially_started in [false, true] {
            let group = uuid::Uuid::new_v4().to_string();
            if initially_started {
                begin(&group, "peer").expect("first").complete(true);
            }
            let old = begin(&group, "peer").expect("old");
            assert!(
                begin(&group, "peer").is_none(),
                "one pending worker per key"
            );
            forget(&group, "peer");
            let new = begin(&group, "peer").expect("new");
            old.complete(true);
            assert_eq!(observed(&group, "peer"), (Duration::ZERO, None));
            new.complete(true);
            assert_eq!(observed(&group, "peer").0, Duration::from_secs(10));
            forget(&group, "peer");
        }
    }
}
