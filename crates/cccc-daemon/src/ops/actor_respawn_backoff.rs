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
//!   - The delay is bounded exponential — a 10s base doubling through
//!     10s, 20s, and 40s, capped at 60s.
//!   - It lives at the DELIVERY-WORKER call site, deliberately not inside
//!     `actor_runtime::apply`. `apply` is also the path for human-initiated
//!     starts (`ops/actors.rs`, `start_group`, the actor CLI verbs); a human
//!     pressing start must happen now, never "in a few seconds because the
//!     last automatic restart failed". Putting the gate in `apply` would delay
//!     people, which is worse than the loop it fixes.
//!   - State is in-memory and per (group, actor). It resets when the actor
//!     reaches healthy uptime, so a one-off crash does not leave a permanent
//!     penalty; a daemon restart forgets it, which is the safe
//!     direction (a fresh daemon should try immediately, not inherit a wait).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// The daemon's automation tick period — the clock the exit/relaunch loop runs
/// on. Named here so the base below is derived from it rather than copied.
///
/// `server.rs` drives an automation pass on this interval
/// (`tokio::time::interval`, `MissedTickBehavior::Skip`), and that pass is what
/// notices an actor process has exited (`prepare_exited` -> `reap_exited` ->
/// `reconcile_exited`). The evidence for the loop's cadence is the burst's
/// timing: every interval was an exact multiple of 5s (5/10/15/20/25), and the
/// stop timestamps held a ~3ms phase band across 20 minutes — a shared ticker,
/// not independently-lifetimed processes.
const AUTOMATION_TICK: Duration = Duration::from_secs(5);

/// First delay after a failed restart, doubling thereafter.
///
/// 5s, NOT the 250ms of `actor_delivery::deferred_retry_delay`: the first delay
/// must exceed ONE AUTOMATION TICK, or the retry is serviced by the same tick
/// that is already driving the loop and nothing changes. A delay below the
/// cadence cannot space the relaunches out — the next message arrives before
/// the wait elapses, so the loop is unchanged and the code only looks like a
/// fix.
///
/// TWO ticks, not one, and the difference is the difference between working and
/// not working: a delay EQUAL to the period can be serviced by the very tick
/// that triggered the loop, so the retry and the loop collide. Twice the period
/// guarantees the relaunch lands after the tick that would have driven it,
/// which is the whole point. (This is also why the measured 4.998s floor was a
/// trap: 5s sits exactly ON the grid rather than off it.)
const RESPAWN_BACKOFF_BASE: Duration = Duration::from_secs(2 * AUTOMATION_TICK.as_secs());
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
    // 10s, 20s, 40s, then capped at 60s. The exponent is clamped before the
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
/// count and waits once — bounded by the 60s cap, and cleared on its first
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

    /// The attempt map is process-global, so tests that touch it must not run
    /// concurrently. With two tests on one key, one test's `forget()` clears the
    /// key the other just asserted on: measured 6/25 parallel failures before
    /// this guard, 0/25 serialized. Every test below that calls `attempts()`
    /// takes this lock AND uses its own key namespace, so a test that later
    /// drops the lock is still unlikely to collide.
    fn test_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    #[test]
    fn no_failures_means_no_wait() {
        assert_eq!(respawn_backoff_delay(0), Duration::ZERO);
    }

    #[test]
    fn the_delay_doubles_then_caps() {
        assert_eq!(respawn_backoff_delay(1), Duration::from_secs(10));
        assert_eq!(respawn_backoff_delay(2), Duration::from_secs(20));
        assert_eq!(respawn_backoff_delay(3), Duration::from_secs(40));
        assert_eq!(respawn_backoff_delay(4), Duration::from_secs(60)); // capped
        assert_eq!(respawn_backoff_delay(5), RESPAWN_BACKOFF_MAX);
        assert_eq!(respawn_backoff_delay(u32::MAX), RESPAWN_BACKOFF_MAX);
    }

    /// The first delay must exceed one automation tick, or it cannot space the
    /// loop out at all: the next tick/message arrives before the wait elapses
    /// and the code only looks like a fix.
    ///
    /// Pinned to the NAMED constant rather than the observed 4.998s floor, so
    /// the invariant documents itself and cannot drift with the measurement.
    /// Fails if anyone lowers the base to or below `AUTOMATION_TICK`.
    #[test]
    fn the_first_delay_exceeds_one_automation_tick() {
        assert!(
            respawn_backoff_delay(1) > AUTOMATION_TICK,
            "the first delay must exceed the daemon's automation tick, otherwise \
             the retry is serviced by the very tick driving the loop"
        );
    }

    /// The base is derived FROM the tick, and is a whole number of ticks, so
    /// the curve stays on the grid instead of drifting off it. If `server.rs`
    /// ever changes its interval this pairing is what to revisit.
    #[test]
    fn the_base_is_a_whole_number_of_automation_ticks() {
        assert_eq!(
            RESPAWN_BACKOFF_BASE.as_secs() % AUTOMATION_TICK.as_secs(),
            0
        );
        assert_eq!(RESPAWN_BACKOFF_BASE, Duration::from_secs(10));
    }

    #[test]
    fn a_successful_restart_clears_the_count() {
        let _guard = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        forget("clear-count", "a");
        record_restart("clear-count", "a", false);
        record_restart("clear-count", "a", false);
        assert!(
            due_delay("clear-count", "a").is_some(),
            "two failures must impose a wait"
        );
        record_restart("clear-count", "a", true);
        assert!(
            due_delay("clear-count", "a").is_none(),
            "a successful restart must clear the penalty"
        );
        forget("clear-count", "a");
    }

    #[test]
    fn failures_are_per_actor_not_global() {
        let _guard = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        forget("per-actor", "a");
        forget("per-actor", "b");
        record_restart("per-actor", "a", false);
        // b has no history, so a dying actor must not delay a healthy one.
        assert!(due_delay("per-actor", "b").is_none());
        assert!(due_delay("per-actor", "a").is_some());
        forget("per-actor", "a");
        forget("per-actor", "b");
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
        let _guard = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        forget("fresh-actor", "fresh");
        assert!(
            due_delay("fresh-actor", "fresh").is_none(),
            "an actor with no failure history must start now, not wait"
        );
    }
}
