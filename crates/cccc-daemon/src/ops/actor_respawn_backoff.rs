//! Bounded backoff for automatic actor restarts.
//!
//! The delivery worker starts a stopped actor when a message arrives for it. If the actor's
//! process exits again soon after, a busy group restarts it on every message. Automatic
//! restarts that begin within `RESPAWN_WINDOW` of the previous automatic restart ending form
//! one loop, and each restart in a loop waits 10 s, 20 s, 40 s, then 60 s. A start, stop,
//! restart or removal by a person clears the history, and so does a daemon restart.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Period of the daemon's automation pass, which notices exited actor processes.
const AUTOMATION_TICK: Duration = Duration::from_secs(5);
const RESPAWN_BACKOFF_BASE: Duration = Duration::from_secs(2 * AUTOMATION_TICK.as_secs());
const RESPAWN_BACKOFF_MAX: Duration = Duration::from_secs(60);
/// A restart that begins this soon after the previous automatic restart ended continues
/// the same loop.
const RESPAWN_WINDOW: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Restarts {
    /// Automatic restarts in the current loop, including the latest one.
    count: u32,
    /// When the latest automatic restart ended, or is planned to begin while it waits.
    last: Instant,
}

type Key = (String, String);

fn restarts() -> &'static Mutex<HashMap<Key, Restarts>> {
    static RESTARTS: OnceLock<Mutex<HashMap<Key, Restarts>>> = OnceLock::new();
    RESTARTS.get_or_init(Default::default)
}

fn key(group_id: &str, actor_id: &str) -> Key {
    (group_id.to_owned(), actor_id.to_owned())
}

fn backoff_delay(earlier_restarts: u32) -> Duration {
    if earlier_restarts == 0 {
        return Duration::ZERO;
    }
    let exponent = earlier_restarts.saturating_sub(1).min(4);
    (RESPAWN_BACKOFF_BASE * (1_u32 << exponent)).min(RESPAWN_BACKOFF_MAX)
}

/// Plan the next automatic restart at `now`: the delay to wait, and the state to keep.
fn plan(previous: Option<Restarts>, now: Instant) -> (Duration, Restarts) {
    let earlier = previous
        .filter(|previous| now.saturating_duration_since(previous.last) < RESPAWN_WINDOW)
        .map_or(0, |previous| previous.count);
    let delay = backoff_delay(earlier);
    let next = Restarts {
        count: earlier.saturating_add(1),
        last: now + delay,
    };
    (delay, next)
}

/// Start an automatic restart of this actor and return how long to wait before launching.
pub fn begin_restart(group_id: &str, actor_id: &str) -> Duration {
    let Ok(mut map) = restarts().lock() else {
        return Duration::ZERO;
    };
    let key = key(group_id, actor_id);
    let (delay, next) = plan(map.get(&key).copied(), Instant::now());
    map.insert(key, next);
    delay
}

/// Mark the end of an automatic restart attempt, whether or not the actor came up.
pub fn end_restart(group_id: &str, actor_id: &str) {
    if let Ok(mut map) = restarts().lock()
        && let Some(entry) = map.get_mut(&key(group_id, actor_id))
    {
        entry.last = Instant::now();
    }
}

/// Forget an actor's automatic restarts, e.g. after a person starts, stops or removes it.
pub fn forget(group_id: &str, actor_id: &str) {
    if let Ok(mut map) = restarts().lock() {
        map.remove(&key(group_id, actor_id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: Duration = Duration::from_secs(1);

    #[test]
    fn the_delay_doubles_then_caps() {
        let delays: Vec<_> = (0..6).map(backoff_delay).collect();
        assert_eq!(delays, [0, 10, 20, 40, 60, 60].map(Duration::from_secs));
        assert_eq!(backoff_delay(u32::MAX), RESPAWN_BACKOFF_MAX);
        assert!(backoff_delay(1) > AUTOMATION_TICK);
    }

    #[test]
    fn restarts_close_together_back_off_and_a_quiet_window_resets() {
        let t0 = Instant::now();
        let (delay, first) = plan(None, t0);
        assert_eq!(delay, Duration::ZERO);
        // The actor came up and exited again 5 s later.
        let (delay, second) = plan(Some(Restarts { last: t0, ..first }), t0 + 5 * SECOND);
        assert_eq!(delay, 10 * SECOND);
        assert_eq!(second.count, 2);
        // A restart more than a window after the last one ended starts a new loop.
        let ended = Restarts {
            last: t0 + 20 * SECOND,
            ..second
        };
        let (delay, fresh) = plan(Some(ended), t0 + 20 * SECOND + RESPAWN_WINDOW + SECOND);
        assert_eq!(delay, Duration::ZERO);
        assert_eq!(fresh.count, 1);
    }

    #[test]
    fn a_long_wait_does_not_reset_the_loop() {
        // Four restarts in a loop: the next one waits the full 60 s.
        let t0 = Instant::now();
        let (delay, planned) = plan(Some(Restarts { count: 4, last: t0 }), t0 + SECOND);
        assert_eq!(delay, RESPAWN_BACKOFF_MAX);
        // The launch after that wait takes 2 s and the actor exits 5 s later. The next
        // restart is still part of the same loop and keeps the capped delay.
        let ended = Restarts {
            last: planned.last + 2 * SECOND,
            ..planned
        };
        let (delay, next) = plan(Some(ended), ended.last + 5 * SECOND);
        assert_eq!(delay, RESPAWN_BACKOFF_MAX);
        assert_eq!(next.count, 6);
    }

    #[test]
    fn history_is_per_actor_and_cleared_by_forget() {
        let group = format!("backoff-test-{}", std::process::id());
        assert_eq!(begin_restart(&group, "dying"), Duration::ZERO);
        end_restart(&group, "dying");
        assert_eq!(begin_restart(&group, "dying"), 10 * SECOND);
        assert_eq!(begin_restart(&group, "healthy"), Duration::ZERO);
        forget(&group, "dying");
        assert_eq!(begin_restart(&group, "dying"), Duration::ZERO);
        forget(&group, "dying");
        forget(&group, "healthy");
    }
}
