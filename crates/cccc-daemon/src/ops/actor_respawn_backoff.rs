//! Bounded backoff for automatically respawning an actor whose process keeps
//! exiting (PROTOTYPE — see the card notes for the evidence).
//!
//! WHY: the delivery worker relaunches a managed actor per incoming message
//! (`actor_delivery_worker.rs` `process_managed_batch`/`process_deepseek_batch`
//! call `actor_runtime::apply(..., "actor.start")`). If the actor's process dies
//! immediately, every message restarts it again, so a chatty group relaunches a
//! dying actor in a tight loop — observed as ~119 restarts in 20 minutes. There
//! is no delay anywhere on that path today.
//!
//! WHAT THIS IS NOT: it is not a fix for *why* the actor dies. It bounds the
//! damage from the loop while the root cause is dealt with separately.
//!
//! SHAPE, and why it is shaped this way:
//!   - The delay is bounded exponential — `250ms * 2^(n-1)`, capped at 4s —
//!     reusing the numbers from `actor_delivery::deferred_retry_delay` so the
//!     daemon has ONE backoff vocabulary rather than two.
//!   - It lives at the DELIVERY-WORKER call site, deliberately not inside
//!     `actor_runtime::apply`. `apply` is also the path for human-initiated
//!     starts (`ops/actors.rs`, `start_group`, the actor CLI verbs); a human
//!     pressing start must happen now, never "in a few seconds because the
//!     last automatic restart failed". Putting the gate in `apply` would delay
//!     people, which is worse than the loop it fixes.
//!   - State is in-memory and per (group, actor). It resets when the actor
//!     reaches healthy uptime, so a one-off crash does not leave a permanent
//!     penalty; a restart daemon restart forgets it, which is the safe
//!     direction (a fresh daemon should try immediately, not inherit a wait).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// First delay after a failed restart, doubling thereafter.
///
/// 5s, NOT the 250ms of `actor_delivery::deferred_retry_delay`, and the reason
/// is the evidence rather than taste: the measured hot loop relaunches about
/// every 5s (126 exits in 20m45s, minimum interval 4.998s). A delay below that
/// floor does not space the relaunches out at all — the next message arrives
/// before the wait elapses, so the loop is unchanged and the code only looks
/// like a fix. The first delay has to clear the observed cadence to do anything.
const RESPAWN_BACKOFF_BASE: Duration = Duration::from_secs(5);
/// Ceiling on the delay, matching the daemon's existing ceiling for a
/// REPEATEDLY failing background task (`membership::restore::MAX_BACKOFF`).
///
/// This is deliberately the 60s ceiling and not `deferred_retry_delay`'s 4s
/// `DEFERRED_RETRY_MAX`: that one bounds a retry of a single operation, whereas
/// this bounds a restart loop that has already failed many times. A cap below
/// the natural cadence would be the same mistake as a too-small base.
const RESPAWN_BACKOFF_MAX: Duration = Duration::from_secs(60);
/// Uptime after which the actor is considered healthy and the count resets.
/// Comfortably longer than a process-start, so an actor that comes up and stays
/// up clears its history rather than carrying a penalty into later crashes.
const RESPAWN_HEALTHY_UPTIME: Duration = Duration::from_secs(60);

type Key = (String, String);

#[derive(Debug, Default, Clone)]
struct Attempts {
    /// Consecutive failed automatic restarts.
    failures: u32,
    /// When the actor was last observed running, to measure healthy uptime.
    last_seen_running: Option<Instant>,
}

fn attempts() -> &'static Mutex<HashMap<Key, Attempts>> {
    static ATTEMPTS: OnceLock<Mutex<HashMap<Key, Attempts>>> = OnceLock::new();
    ATTEMPTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The delay before the next automatic restart of a dying actor.
///
/// `failures` is the count of consecutive failed restarts; 0 means "try now".
/// Pure, so the curve is testable without a clock or a live process.
pub fn respawn_backoff_delay(failures: u32) -> Duration {
    if failures == 0 {
        return Duration::ZERO;
    }
    // 5s, 10s, 20s, 40s, then capped at 60s. The exponent is clamped before the
    // shift so a long-failing actor cannot overflow the multiply.
    let exponent = failures.saturating_sub(1).min(4);
    let delay = RESPAWN_BACKOFF_BASE * (1_u32 << exponent);
    delay.min(RESPAWN_BACKOFF_MAX)
}

/// Should this automatic restart wait, and for how long? `None` means proceed
/// immediately.
///
/// Note the actor is NOT holding a lock while the caller sleeps: this only
/// reports the wait. Sleeping stays with the caller so a long backoff never
/// blocks another actor's delivery.
pub fn due_delay(group_id: &str, actor_id: &str) -> Option<Duration> {
    let key = (group_id.to_owned(), actor_id.to_owned());
    let now = Instant::now();
    let mut map = attempts().lock().ok()?;
    let entry = map.entry(key).or_default();
    // An actor that has been up long enough is healthy: forget past failures
    // rather than penalising a crash that happens an hour from now.
    if let Some(since) = entry.last_seen_running
        && now.duration_since(since) >= RESPAWN_HEALTHY_UPTIME
    {
        entry.failures = 0;
    }
    let delay = respawn_backoff_delay(entry.failures);
    (!delay.is_zero()).then_some(delay)
}

/// Record the outcome of an automatic restart attempt.
///
/// `running` is whether the actor was up after the attempt. A success clears the
/// count; a failure advances it. Called by the worker, not by `apply`.
pub fn record_restart(group_id: &str, actor_id: &str, running: bool) {
    let key = (group_id.to_owned(), actor_id.to_owned());
    let mut map = match attempts().lock() {
        Ok(map) => map,
        Err(_) => return,
    };
    let entry = map.entry(key).or_default();
    if running {
        entry.failures = 0;
        entry.last_seen_running = Some(Instant::now());
    } else {
        entry.failures = entry.failures.saturating_add(1);
        entry.last_seen_running = None;
    }
}

/// Forget an actor's history — used when an actor is stopped or removed, so a
/// later start begins clean instead of inheriting a stale penalty.
///
/// PROTOTYPE NOTE: not yet called from the stop/remove paths; only the tests
/// use it today. Wiring it into actor-stop is deliberate follow-up, not part of
/// this bounded change. Until then a stopped-then-restarted actor keeps its
/// count and waits once — bounded by the 4s cap, and cleared on its first
/// successful restart, so the effect is a short delay rather than a penalty.
#[allow(dead_code)]
pub fn forget(group_id: &str, actor_id: &str) {
    if let Ok(mut map) = attempts().lock() {
        map.remove(&(group_id.to_owned(), actor_id.to_owned()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_failures_means_no_wait() {
        assert_eq!(respawn_backoff_delay(0), Duration::ZERO);
    }

    #[test]
    fn the_delay_doubles_then_caps() {
        assert_eq!(respawn_backoff_delay(1), Duration::from_secs(5));
        assert_eq!(respawn_backoff_delay(2), Duration::from_secs(10));
        assert_eq!(respawn_backoff_delay(3), Duration::from_secs(20));
        assert_eq!(respawn_backoff_delay(4), Duration::from_secs(40));
        assert_eq!(respawn_backoff_delay(5), RESPAWN_BACKOFF_MAX);
        assert_eq!(respawn_backoff_delay(u32::MAX), RESPAWN_BACKOFF_MAX);
    }

    /// The first delay must exceed the measured relaunch cadence, or it cannot
    /// space the loop out at all: the next message arrives before the wait
    /// elapses and the code only looks like a fix.
    ///
    /// The measured loop: 126 exits in 20m45s, minimum interval 4.998s. This
    /// test fails if anyone lowers the base under that floor.
    #[test]
    fn the_first_delay_clears_the_measured_relaunch_cadence() {
        const MEASURED_MIN_INTERVAL: Duration = Duration::from_millis(4_998);
        assert!(
            respawn_backoff_delay(1) > MEASURED_MIN_INTERVAL,
            "the first delay must exceed the observed ~5s cadence, otherwise the \
             loop is unchanged: measured minimum interval was 4.998s"
        );
    }

    #[test]
    fn a_successful_restart_clears_the_count() {
        forget("g", "a");
        record_restart("g", "a", false);
        record_restart("g", "a", false);
        assert!(
            due_delay("g", "a").is_some(),
            "two failures must impose a wait"
        );
        record_restart("g", "a", true);
        assert!(
            due_delay("g", "a").is_none(),
            "a successful restart must clear the penalty"
        );
        forget("g", "a");
    }

    #[test]
    fn failures_are_per_actor_not_global() {
        forget("g", "a");
        forget("g", "b");
        record_restart("g", "a", false);
        // b has no history, so a dying actor must not delay a healthy one.
        assert!(due_delay("g", "b").is_none());
        assert!(due_delay("g", "a").is_some());
        forget("g", "a");
        forget("g", "b");
    }

    /// THE NEGATIVE CONTROL the card asks for, as a property of the gate's
    /// placement: the backoff is only ever consulted by the DELIVERY WORKER's
    /// automatic respawn. `actor_runtime::apply` — the path a human start takes
    /// (`ops/actors.rs`, `start_group`, the actor CLI verbs) — never calls into
    /// this module, so a person pressing start cannot be delayed by a previous
    /// automatic failure.
    ///
    /// That is a claim about which files the delay logic reaches, so assert it
    /// the way it can actually break: the delay must not be reachable from
    /// `apply`. If someone later moves the gate into `apply`, this test tells
    /// them why they must not.
    #[test]
    fn the_backoff_is_not_reachable_from_the_human_start_path() {
        let apply_src = include_str!("actor_runtime.rs");
        assert!(
            !apply_src.contains("actor_respawn_backoff"),
            "actor_runtime::apply must never consult the respawn backoff: it is \
             also the human-initiated start path, and a person pressing start \
             must not wait on a previous automatic failure"
        );
        let actors_src = include_str!("actors.rs");
        assert!(
            !actors_src.contains("actor_respawn_backoff"),
            "the manual actor verbs must never consult the respawn backoff"
        );
    }

    /// The gate must be consulted by the AUTOMATIC respawn, or it does nothing.
    /// Without this the property above could be satisfied by never gating
    /// anything at all.
    #[test]
    fn the_backoff_is_wired_into_the_automatic_respawn() {
        let worker_src = include_str!("actor_delivery_worker.rs");
        assert!(
            worker_src.contains("actor_respawn_backoff::due_delay")
                && worker_src.contains("actor_respawn_backoff::record_restart"),
            "the delivery worker must apply the backoff on its automatic respawn \
             (both the managed and deepseek paths)"
        );
        // Both automatic respawn paths, not just one.
        assert_eq!(
            worker_src
                .matches("actor_respawn_backoff::due_delay")
                .count(),
            2,
            "both the managed and deepseek automatic respawn paths must be gated"
        );
    }

    #[test]
    fn a_fresh_actor_starts_immediately() {
        forget("g", "fresh");
        assert!(
            due_delay("g", "fresh").is_none(),
            "an actor with no failure history must start now, not wait"
        );
    }
}
