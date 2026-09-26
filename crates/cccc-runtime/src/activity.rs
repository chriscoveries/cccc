//! Per-session terminal activity: when the PTY last produced output, when a
//! delivery last reached its input, and how many output bytes arrived recently.
//!
//! The daemon owns the PTY master, so these are observed facts rather than
//! screen scraping. Interpretation (working / idle) belongs to the caller.
//! Each `Session` owns a fresh tracker, so start and restart reset it.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

/// Output byte counts are kept in one-second buckets for this long. Callers can
/// ask about any window up to this length.
pub const ACTIVITY_RETENTION: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActivityInstant {
    /// Wall-clock time, for reporting.
    pub at: SystemTime,
    /// Elapsed time since the event, measured on the monotonic clock.
    pub age: Duration,
}

/// A point-in-time copy of a session's activity counters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActivitySnapshot {
    pub last_output: Option<ActivityInstant>,
    pub last_input: Option<ActivityInstant>,
    /// `(age, bytes)` per one-second bucket, newest first, within `ACTIVITY_RETENTION`.
    pub recent_output: Vec<(Duration, u64)>,
    /// Output bytes observed since the most recent input delivery.
    pub output_bytes_since_input: u64,
}

impl ActivitySnapshot {
    /// Output bytes observed within `window` of the snapshot.
    pub fn output_bytes_within(&self, window: Duration) -> u64 {
        self.recent_output
            .iter()
            .filter(|(age, _)| *age <= window)
            .map(|(_, bytes)| *bytes)
            .sum()
    }
}

#[derive(Debug)]
struct State {
    origin: Instant,
    last_output: Option<(Instant, SystemTime)>,
    last_input: Option<(Instant, SystemTime)>,
    /// `(seconds since origin, bytes)`, oldest first.
    buckets: VecDeque<(u64, u64)>,
    output_bytes_since_input: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct ActivityTracker(Arc<Mutex<State>>);

impl Default for ActivityTracker {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(State {
            origin: Instant::now(),
            last_output: None,
            last_input: None,
            buckets: VecDeque::new(),
            output_bytes_since_input: 0,
        })))
    }
}

impl ActivityTracker {
    fn lock(&self) -> MutexGuard<'_, State> {
        // Counters only: a panic elsewhere must not stop activity reporting.
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn record_output(&self, bytes: usize) {
        if bytes == 0 {
            return;
        }
        let now = Instant::now();
        let bytes = bytes as u64;
        let mut state = self.lock();
        let second = now.duration_since(state.origin).as_secs();
        match state.buckets.back_mut() {
            Some((last, total)) if *last == second => *total = total.saturating_add(bytes),
            _ => state.buckets.push_back((second, bytes)),
        }
        let oldest = second.saturating_sub(ACTIVITY_RETENTION.as_secs());
        while state.buckets.front().is_some_and(|(s, _)| *s < oldest) {
            state.buckets.pop_front();
        }
        state.last_output = Some((now, SystemTime::now()));
        state.output_bytes_since_input = state.output_bytes_since_input.saturating_add(bytes);
    }

    pub(crate) fn record_input(&self) {
        let mut state = self.lock();
        state.last_input = Some((Instant::now(), SystemTime::now()));
        state.output_bytes_since_input = 0;
    }

    pub(crate) fn snapshot(&self) -> ActivitySnapshot {
        let now = Instant::now();
        let state = self.lock();
        let current = now.duration_since(state.origin).as_secs();
        let instant = |(at, wall): (Instant, SystemTime)| ActivityInstant {
            at: wall,
            age: now.saturating_duration_since(at),
        };
        ActivitySnapshot {
            last_output: state.last_output.map(instant),
            last_input: state.last_input.map(instant),
            recent_output: state
                .buckets
                .iter()
                .rev()
                .map(|(second, bytes)| {
                    (Duration::from_secs(current.saturating_sub(*second)), *bytes)
                })
                .filter(|(age, _)| *age <= ACTIVITY_RETENTION)
                .collect(),
            output_bytes_since_input: state.output_bytes_since_input,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_tracker_has_no_activity() {
        let snapshot = ActivityTracker::default().snapshot();
        assert_eq!(snapshot, ActivitySnapshot::default());
        assert_eq!(snapshot.output_bytes_within(Duration::from_secs(30)), 0);
    }

    #[test]
    fn output_accumulates_and_input_resets_since_input_count() {
        let tracker = ActivityTracker::default();
        tracker.record_output(100);
        tracker.record_output(28);
        tracker.record_output(0);
        let snapshot = tracker.snapshot();
        assert!(snapshot.last_output.is_some());
        assert!(snapshot.last_input.is_none());
        assert_eq!(snapshot.output_bytes_within(Duration::from_secs(30)), 128);
        assert_eq!(snapshot.output_bytes_since_input, 128);

        tracker.record_input();
        tracker.record_output(5);
        let snapshot = tracker.snapshot();
        assert!(snapshot.last_input.is_some());
        assert_eq!(snapshot.output_bytes_since_input, 5);
        assert_eq!(snapshot.output_bytes_within(Duration::from_secs(30)), 133);
    }
}
