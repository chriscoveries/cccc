//! Durable restart backoff and parking for managed Actor runtimes (RS-3).
//!
//! A provider that dies before its first completed turn is a fast failure. Repeated fast
//! failures are what turn one bad launch into a hot loop, so the count and the resulting
//! delay live in the Group's durable state beside the lifecycle ledger, not in process
//! memory: a daemon restart must not hand a doomed actor a clean slate.
//!
//! The gate belongs to the automation path only. `actor_runtime::apply` is also the human
//! start path, and a person pressing start must never wait out an automation backoff or be
//! refused because the actor is parked; manual starts call [`clear_for_manual_start`] first.

use cccc_contracts::{Event, utc_now};
use cccc_core::{GroupStore, HomeLayout, fs, ledger};
use serde_json::{Map, Value, json};
use std::io;
use std::path::PathBuf;
use std::time::Duration;

/// A launch that never reached its admission window is a fast failure. Turn completion is per
/// protocol, so the daemon observes admission as uptime: a managed session that outlives this
/// window reached its first turn, and one that exits sooner did not.
pub(crate) const FAST_FAILURE_WINDOW: Duration = Duration::from_secs(30);
/// Bounded exponential delays: 2s, 4s, 8s, 16s, then the 30s cap.
pub(crate) const BACKOFF_DELAYS: [Duration; 5] = [
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(16),
    Duration::from_secs(30),
];
/// Consecutive fast failures after which the actor is parked and automation stops retrying.
pub(crate) const MAX_FAST_FAILURES: u32 = 5;
const STATE_VERSION: u64 = 1;
const MAX_STDERR: usize = 512;
const PARKED_FAST_FAILURES: &str = "consecutive_fast_failures";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RestartState {
    pub consecutive_fast_failures: u32,
    pub next_restart_at: Option<i64>,
    pub last_exit_code: Option<i64>,
    pub last_exit_stderr: String,
    pub parked_reason: String,
}

impl RestartState {
    #[must_use]
    pub(crate) fn parked(&self) -> bool {
        !self.parked_reason.is_empty()
    }

    /// The delay the next automatic restart would take, or `None` when a restart may proceed now.
    #[must_use]
    pub(crate) fn remaining_delay(&self, now_ms: i64) -> Option<Duration> {
        let until = self.next_restart_at?;
        let remaining = until - now_ms;
        (remaining > 0).then(|| Duration::from_millis(remaining as u64))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Gate {
    Allow,
    /// Automatic restarts are held until `at_ms`; a manual start ignores this.
    Wait {
        at_ms: i64,
        delay: Duration,
    },
    Parked {
        reason: String,
    },
}

#[must_use]
pub(crate) fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// How long a managed session ran, from the runtime's own `started_at`.
#[must_use]
pub(crate) fn uptime_ms(started_at: &str, now_ms: i64) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(started_at.trim())
        .ok()
        .map(|started| now_ms - started.timestamp_millis())
}

/// A provider that exited inside the admission window is a fast failure; one that outlived it
/// reached its first completed turn, so the launch counts as good.
#[must_use]
pub(crate) fn is_fast_failure(started_at: &str, now_ms: i64) -> bool {
    match uptime_ms(started_at, now_ms) {
        Some(uptime) => uptime < FAST_FAILURE_WINDOW.as_millis() as i64,
        // An unparsable start time is not evidence of a fast failure.
        None => false,
    }
}

/// What automation may do with an actor that is not running.
#[must_use]
pub(crate) fn gate(home: &HomeLayout, group_id: &str, actor_id: &str) -> Gate {
    let state = load(home, group_id, actor_id).unwrap_or_default();
    gate_state(&state, now_ms())
}

#[must_use]
pub(crate) fn gate_state(state: &RestartState, now_ms: i64) -> Gate {
    if state.parked() {
        return Gate::Parked {
            reason: state.parked_reason.clone(),
        };
    }
    match state.remaining_delay(now_ms) {
        Some(delay) => Gate::Wait {
            at_ms: state.next_restart_at.unwrap_or_default(),
            delay,
        },
        None => Gate::Allow,
    }
}

/// A provider exit before the first completed turn. Delays the next automatic restart, or parks
/// the actor once the failure count reaches the cap.
pub(crate) fn record_fast_failure(
    home: &HomeLayout,
    group_id: &str,
    actor_id: &str,
    exit_code: Option<i32>,
    stderr: &str,
) -> io::Result<RestartState> {
    let mut state = load(home, group_id, actor_id)?;
    state.consecutive_fast_failures = state.consecutive_fast_failures.saturating_add(1);
    state.last_exit_code = exit_code.map(i64::from);
    state.last_exit_stderr = truncate(stderr);
    let now = now_ms();
    if state.consecutive_fast_failures >= MAX_FAST_FAILURES {
        state.next_restart_at = None;
        // Already parked: refresh the observed failure, but do not re-announce the park. A parked
        // actor is not retried, so a second actor.parked event would only be noise.
        if state.parked() {
            return commit(home, group_id, actor_id, &state, None);
        }
        state.parked_reason = format!("{PARKED_FAST_FAILURES}={}", state.consecutive_fast_failures);
        let event = parked_event(group_id, actor_id, &state);
        return commit(home, group_id, actor_id, &state, Some(event));
    }
    match hold_delay(state.consecutive_fast_failures) {
        Some(delay) => {
            state.next_restart_at = Some(now + delay.as_millis() as i64);
            let event = backoff_event(group_id, actor_id, &state, delay);
            commit(home, group_id, actor_id, &state, Some(event))
        }
        None => {
            // First failure: recorded, visible, and not held back.
            state.next_restart_at = None;
            commit(home, group_id, actor_id, &state, None)
        }
    }
}

/// The first completed turn proves the launch worked: clear the failure count and any park.
pub(crate) fn record_first_turn(
    home: &HomeLayout,
    group_id: &str,
    actor_id: &str,
) -> io::Result<()> {
    let state = load(home, group_id, actor_id)?;
    if state.consecutive_fast_failures == 0 && state.next_restart_at.is_none() && !state.parked() {
        return Ok(());
    }
    let cleared = RestartState::default();
    commit(home, group_id, actor_id, &cleared, None).map(|_| ())
}

/// A manual start or other explicit operator action clears the park and opens a new epoch.
pub(crate) fn clear_for_manual_start(
    home: &HomeLayout,
    group_id: &str,
    actor_id: &str,
) -> io::Result<RestartState> {
    let state = load(home, group_id, actor_id)?;
    if !state.parked() && state.consecutive_fast_failures == 0 && state.next_restart_at.is_none() {
        return Ok(state);
    }
    let mut event = Event::new("actor.unparked", group_id);
    event.by = "system".into();
    event.data = json!({
        "actor_id": actor_id,
        "previous_parked_reason": state.parked_reason,
        "previous_consecutive_fast_failures": state.consecutive_fast_failures,
    })
    .as_object()
    .cloned()
    .unwrap_or_default();
    let cleared = RestartState::default();
    commit(home, group_id, actor_id, &cleared, Some(event))
}

/// How long the next automatic restart is held after `consecutive_fast_failures` failures.
///
/// The first failure is not held back: a crash followed by a directed message is the ordinary path,
/// and a single short delay cannot break a relaunch loop — it only adds latency to every
/// legitimate wake, including the daemon's own auto-wake contract. The ladder therefore starts at
/// the second consecutive failure, and the cap is reached before the park.
#[must_use]
pub(crate) fn hold_delay(consecutive_fast_failures: u32) -> Option<Duration> {
    if consecutive_fast_failures < 2 {
        return None;
    }
    let index = (consecutive_fast_failures as usize - 2).min(BACKOFF_DELAYS.len() - 1);
    Some(BACKOFF_DELAYS[index])
}

pub(crate) fn load(home: &HomeLayout, group_id: &str, actor_id: &str) -> io::Result<RestartState> {
    let path = state_path(home, group_id, actor_id)?;
    let Ok(document) = fs::read_json::<Value>(&path) else {
        return Ok(RestartState::default());
    };
    // Only a state whose ledger append did not complete pays for the recovery read.
    if document
        .get("pending_event")
        .is_some_and(|value| value.is_object())
    {
        recover_pending_event(home, group_id, actor_id, &document);
    }
    Ok(from_document(&document))
}

fn from_document(document: &Value) -> RestartState {
    RestartState {
        consecutive_fast_failures: document
            .get("consecutive_fast_failures")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
        next_restart_at: document.get("next_restart_at_ms").and_then(Value::as_i64),
        last_exit_code: document.get("last_exit_code").and_then(Value::as_i64),
        last_exit_stderr: document
            .get("last_exit_stderr")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        parked_reason: document
            .get("parked_reason")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    }
}

fn to_document(state: &RestartState, pending_event: Option<&Value>) -> Map<String, Value> {
    Map::from_iter([
        ("v".into(), json!(STATE_VERSION)),
        ("kind".into(), json!("actor_restart_state")),
        (
            "consecutive_fast_failures".into(),
            json!(state.consecutive_fast_failures),
        ),
        ("next_restart_at_ms".into(), json!(state.next_restart_at)),
        (
            "next_restart_at".into(),
            json!(
                state
                    .next_restart_at
                    .and_then(chrono::DateTime::from_timestamp_millis)
                    .map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
            ),
        ),
        ("last_exit_code".into(), json!(state.last_exit_code)),
        ("last_exit_stderr".into(), json!(state.last_exit_stderr)),
        ("parked_reason".into(), json!(state.parked_reason)),
        (
            "pending_event".into(),
            pending_event.cloned().unwrap_or(Value::Null),
        ),
        ("updated_at".into(), json!(utc_now())),
    ])
}

/// Persist the state and its ledger event as one step: the event is written into the state
/// document first, so a crash between the two writes is repaired on the next load instead of
/// leaving a parked actor with no visible reason, or a delay with no record.
fn commit(
    home: &HomeLayout,
    group_id: &str,
    actor_id: &str,
    state: &RestartState,
    event: Option<Event>,
) -> io::Result<RestartState> {
    let store = GroupStore::new(home.clone())?;
    let path = state_path(home, group_id, actor_id)?;
    let payload = event.as_ref().map(|event| {
        let mut value = serde_json::to_value(event).unwrap_or(Value::Null);
        if let Some(map) = value.as_object_mut() {
            map.insert("id".into(), json!(event.id));
        }
        value
    });
    fs::write_json(&path, &to_document(state, payload.as_ref()))?;
    if let Some(event) = event {
        // A read-back failure here must not undo the state write; the pending_event payload in
        // the document is the recovery path.
        if let Err(error) = ledger::append(&store.ledger_path(group_id)?, &event) {
            tracing::warn!(
                %group_id, %actor_id, %error,
                "restart state committed without its ledger event; event replays on next load"
            );
        } else if let Err(error) = clear_pending_event(&path) {
            // The event is durable; only the recovery marker is stale, so a replay is a no-op.
            tracing::warn!(
                %group_id, %actor_id, %error,
                "restart ledger event recorded but its recovery marker could not be cleared"
            );
        }
    }
    Ok(state.clone())
}

fn clear_pending_event(path: &std::path::Path) -> io::Result<()> {
    let mut document = fs::read_json::<Value>(path)?;
    if let Some(map) = document.as_object_mut() {
        map.insert("pending_event".into(), Value::Null);
    }
    fs::write_json(path, &document)
}

/// Replay a state write whose ledger append did not complete before the crash.
fn recover_pending_event(home: &HomeLayout, group_id: &str, actor_id: &str, document: &Value) {
    let Some(pending) = document
        .get("pending_event")
        .filter(|value| value.is_object())
    else {
        return;
    };
    let Ok(event) = serde_json::from_value::<Event>(pending.clone()) else {
        return;
    };
    let Ok(store) = GroupStore::new(home.clone()) else {
        return;
    };
    let Ok(ledger_path) = store.ledger_path(group_id) else {
        return;
    };
    if ledger::read_all(&ledger_path)
        .unwrap_or_default()
        .iter()
        .any(|existing| existing.id == event.id)
    {
        // Already durable; drop the stale marker.
        if let Ok(path) = state_path(home, group_id, actor_id) {
            let _ = clear_pending_event(&path);
        }
        return;
    }
    if let Err(error) = ledger::append(&ledger_path, &event) {
        tracing::warn!(%group_id, %actor_id, %error, "failed to replay pending restart event");
        return;
    }
    if let Ok(path) = state_path(home, group_id, actor_id)
        && let Err(error) = clear_pending_event(&path)
    {
        tracing::warn!(%group_id, %actor_id, %error, "replayed restart event; marker not cleared");
    }
}

fn parked_event(group_id: &str, actor_id: &str, state: &RestartState) -> Event {
    let mut event = Event::new("actor.parked", group_id);
    event.by = "system".into();
    event.data = json!({
        "actor_id": actor_id,
        "reason": state.parked_reason,
        "exit_code": state.last_exit_code,
        "stderr": state.last_exit_stderr,
        "consecutive_fast_failures": state.consecutive_fast_failures,
    })
    .as_object()
    .cloned()
    .unwrap_or_default();
    event
}

fn backoff_event(group_id: &str, actor_id: &str, state: &RestartState, delay: Duration) -> Event {
    let mut event = Event::new("actor.backoff", group_id);
    event.by = "system".into();
    event.data = json!({
        "actor_id": actor_id,
        "consecutive_fast_failures": state.consecutive_fast_failures,
        "delay_ms": delay.as_millis() as u64,
        "next_restart_at_ms": state.next_restart_at,
        "exit_code": state.last_exit_code,
        "stderr": state.last_exit_stderr,
    })
    .as_object()
    .cloned()
    .unwrap_or_default();
    event
}

fn truncate(stderr: &str) -> String {
    stderr.chars().take(MAX_STDERR).collect()
}

fn state_path(home: &HomeLayout, group_id: &str, actor_id: &str) -> io::Result<PathBuf> {
    if !safe_actor_id(actor_id) {
        return Err(io::Error::other("invalid actor id"));
    }
    Ok(GroupStore::new(home.clone())?
        .state_dir(group_id)?
        .join("actor_restart_states")
        .join(format!("{actor_id}.json")))
}

fn safe_actor_id(actor_id: &str) -> bool {
    actor_id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        || (!actor_id.is_empty()
            && !actor_id.contains(['/', '\\'])
            && actor_id != "."
            && actor_id != "..")
}

#[cfg(test)]
mod tests {
    use super::*;
    use cccc_core::GroupStore;

    fn fixture() -> (tempfile::TempDir, HomeLayout, String) {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let group = GroupStore::new(home.clone())
            .expect("store")
            .create("restart backoff", "")
            .expect("group");
        (temp, home, group.group_id)
    }

    fn events(home: &HomeLayout, group_id: &str) -> Vec<Event> {
        let store = GroupStore::new(home.clone()).expect("store");
        ledger::read_all(&store.ledger_path(group_id).expect("path")).expect("read ledger")
    }

    #[test]
    fn the_first_failure_is_recorded_but_not_held_back() {
        assert_eq!(hold_delay(0), None);
        assert_eq!(
            hold_delay(1),
            None,
            "the ordinary crash-and-wake path is not delayed"
        );
        assert_eq!(hold_delay(2), Some(Duration::from_secs(2)));
        assert_eq!(hold_delay(3), Some(Duration::from_secs(4)));
        assert_eq!(hold_delay(4), Some(Duration::from_secs(8)));
        assert_eq!(hold_delay(5), Some(Duration::from_secs(16)));
        assert_eq!(
            hold_delay(50),
            Some(Duration::from_secs(30)),
            "the cap holds"
        );
    }

    #[test]
    fn fast_failures_delay_then_park_with_visible_reason() {
        let (_temp, home, group_id) = fixture();
        let first = record_fast_failure(&home, &group_id, "peer1", Some(1), "provider exited")
            .expect("record first failure");
        assert_eq!(first.consecutive_fast_failures, 1);
        assert!(
            first.next_restart_at.is_none(),
            "one crash must not delay the next legitimate wake"
        );
        assert_eq!(
            gate(&home, &group_id, "peer1"),
            Gate::Allow,
            "a single failure leaves automation free to wake the actor"
        );
        for expected in 2..MAX_FAST_FAILURES {
            let state = record_fast_failure(&home, &group_id, "peer1", Some(1), "provider exited")
                .expect("record fast failure");
            assert_eq!(state.consecutive_fast_failures, expected);
            assert!(!state.parked(), "not parked before the cap");
            assert!(state.next_restart_at.is_some_and(|at| at > now_ms()));
        }
        let parked = record_fast_failure(&home, &group_id, "peer1", Some(1), "provider exited")
            .expect("record parking failure");
        assert_eq!(parked.consecutive_fast_failures, MAX_FAST_FAILURES);
        assert!(parked.parked());
        assert_eq!(parked.parked_reason, "consecutive_fast_failures=5");
        assert_eq!(
            parked.next_restart_at, None,
            "a parked actor has no next restart"
        );

        match gate(&home, &group_id, "peer1") {
            Gate::Parked { reason } => assert_eq!(reason, "consecutive_fast_failures=5"),
            other => panic!("parked actor must not be relaunched automatically: {other:?}"),
        }

        let parked_events = events(&home, &group_id)
            .into_iter()
            .filter(|event| event.kind == "actor.parked")
            .collect::<Vec<_>>();
        assert_eq!(
            parked_events.len(),
            1,
            "one parking event, not one per attempt"
        );
        assert_eq!(parked_events[0].by, "system");
        assert_eq!(parked_events[0].data["actor_id"], json!("peer1"));
        assert_eq!(parked_events[0].data["exit_code"], json!(1));
        assert_eq!(parked_events[0].data["stderr"], json!("provider exited"));
        assert_eq!(
            parked_events[0].data["reason"],
            json!("consecutive_fast_failures=5")
        );

        // A sixth failure must not emit a second parking event.
        record_fast_failure(&home, &group_id, "peer1", Some(1), "again").expect("sixth failure");
        assert_eq!(
            events(&home, &group_id)
                .iter()
                .filter(|event| event.kind == "actor.parked")
                .count(),
            1
        );
    }

    #[test]
    fn first_turn_clears_the_count_and_the_park() {
        let (_temp, home, group_id) = fixture();
        for _ in 0..MAX_FAST_FAILURES {
            record_fast_failure(&home, &group_id, "peer1", Some(1), "boom").expect("failure");
        }
        assert!(matches!(
            gate(&home, &group_id, "peer1"),
            Gate::Parked { .. }
        ));

        record_first_turn(&home, &group_id, "peer1").expect("first turn");

        let state = load(&home, &group_id, "peer1").expect("load");
        assert_eq!(
            state,
            RestartState::default(),
            "a working launch clears the epoch"
        );
        assert_eq!(gate(&home, &group_id, "peer1"), Gate::Allow);
    }

    #[test]
    fn manual_start_clears_the_park_and_opens_a_new_epoch() {
        let (_temp, home, group_id) = fixture();
        for _ in 0..MAX_FAST_FAILURES {
            record_fast_failure(&home, &group_id, "peer1", Some(2), "boom").expect("failure");
        }
        clear_for_manual_start(&home, &group_id, "peer1").expect("manual start");
        assert_eq!(gate(&home, &group_id, "peer1"), Gate::Allow);
        let unparked = events(&home, &group_id)
            .into_iter()
            .find(|event| event.kind == "actor.unparked")
            .expect("unpark event");
        assert_eq!(
            unparked.data["previous_parked_reason"],
            json!("consecutive_fast_failures=5")
        );

        // The new epoch counts from zero, so four more failures delay instead of parking.
        for _ in 0..4 {
            record_fast_failure(&home, &group_id, "peer1", Some(2), "boom").expect("failure");
        }
        assert!(matches!(gate(&home, &group_id, "peer1"), Gate::Wait { .. }));
    }

    #[test]
    fn gate_holds_automatic_restarts_until_the_delay_elapses() {
        let state = RestartState {
            consecutive_fast_failures: 1,
            next_restart_at: Some(now_ms() + 1_500),
            ..RestartState::default()
        };
        assert_eq!(
            gate_state(&state, now_ms()),
            Gate::Wait {
                at_ms: state.next_restart_at.unwrap_or_default(),
                delay: Duration::from_millis(1_500)
            }
        );
        assert_eq!(
            gate_state(&state, state.next_restart_at.unwrap_or_default() + 1),
            Gate::Allow,
            "the gate opens once the delay has elapsed"
        );
    }

    #[test]
    fn state_survives_a_daemon_restart() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home_path = temp.path().join("home");
        let group_id = {
            let home = HomeLayout::from_path(home_path.clone()).expect("home");
            let group = GroupStore::new(home.clone())
                .expect("store")
                .create("durable", "")
                .expect("group");
            group.group_id
        };
        for _ in 0..MAX_FAST_FAILURES {
            let home = HomeLayout::from_path(home_path.clone()).expect("home");
            record_fast_failure(&home, &group_id, "peer1", Some(9), "dies immediately")
                .expect("failure");
        }
        // A fresh HomeLayout is what a restarted daemon sees; the cap and reason must survive.
        let restarted = HomeLayout::from_path(home_path).expect("home");
        let state = load(&restarted, &group_id, "peer1").expect("load after restart");
        assert!(state.parked());
        assert_eq!(state.consecutive_fast_failures, MAX_FAST_FAILURES);
        assert_eq!(state.parked_reason, "consecutive_fast_failures=5");
        assert_eq!(state.last_exit_code, Some(9));
        assert!(matches!(
            gate(&restarted, &group_id, "peer1"),
            Gate::Parked { .. }
        ));
    }

    #[test]
    fn unacknowledged_event_is_replayed_once_after_a_crash() {
        let (_temp, home, group_id) = fixture();
        // Two failures: the second one schedules a backoff and therefore records a ledger event.
        record_fast_failure(&home, &group_id, "peer1", Some(1), "boom").expect("first failure");
        record_fast_failure(&home, &group_id, "peer1", Some(1), "boom").expect("second failure");
        // Simulate the crash between the state write and the ledger append.
        let path = state_path(&home, &group_id, "peer1").expect("path");
        let mut document = fs::read_json::<Value>(&path).expect("read state");
        let pending = events(&home, &group_id)
            .last()
            .cloned()
            .expect("the failure was recorded in the ledger");
        let pending = serde_json::to_value(&pending).expect("event value");
        let ledger_path = GroupStore::new(home.clone())
            .expect("store")
            .ledger_path(&group_id)
            .expect("ledger path");
        std::fs::write(&ledger_path, "").expect("truncate ledger");
        document
            .as_object_mut()
            .expect("object")
            .insert("pending_event".into(), pending);
        fs::write_json(&path, &document).expect("rewrite state");

        // A load is what repairs it, so the replay is driven through the public path.
        let _ = load(&home, &group_id, "peer1").expect("load");
        let first = events(&home, &group_id).len();
        assert_eq!(first, 1, "the pending event is replayed on load");
        let _ = load(&home, &group_id, "peer1").expect("second load");
        assert_eq!(
            events(&home, &group_id).len(),
            first,
            "replay is idempotent once the marker is cleared"
        );
        let document = fs::read_json::<Value>(&path).expect("read state");
        assert!(
            document["pending_event"].is_null(),
            "a recovered state carries no stale recovery marker"
        );
    }

    #[test]
    fn an_exit_inside_the_admission_window_is_a_fast_failure() {
        let now = now_ms();
        let fresh = chrono::DateTime::from_timestamp_millis(now - 3_000)
            .expect("timestamp")
            .to_rfc3339();
        let established = chrono::DateTime::from_timestamp_millis(now - 120_000)
            .expect("timestamp")
            .to_rfc3339();
        assert!(is_fast_failure(&fresh, now), "3s uptime is a fast failure");
        assert!(
            !is_fast_failure(&established, now),
            "a session that outlived the window reached its first turn"
        );
        assert!(
            !is_fast_failure("not-a-timestamp", now),
            "an unparsable start time is not evidence of a fast failure"
        );
        assert_eq!(
            uptime_ms(&established, now),
            Some(120_000),
            "uptime comes from the runtime's own start time"
        );
    }

    #[test]
    fn stderr_is_bounded() {
        let (_temp, home, group_id) = fixture();
        let noisy = "x".repeat(MAX_STDERR * 2);
        let state =
            record_fast_failure(&home, &group_id, "peer1", Some(1), &noisy).expect("failure");
        assert_eq!(state.last_exit_stderr.chars().count(), MAX_STDERR);
    }

    #[test]
    fn unknown_actor_state_is_empty_rather_than_an_error() {
        let (_temp, home, group_id) = fixture();
        assert_eq!(
            load(&home, &group_id, "nobody").expect("load"),
            RestartState::default()
        );
        assert_eq!(gate(&home, &group_id, "nobody"), Gate::Allow);
        assert!(state_path(&home, &group_id, "../escape").is_err());
    }
}
