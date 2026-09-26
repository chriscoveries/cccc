use super::working_state::{PtyLivenessConfig, fields, pty_liveness};
use cccc_contracts::{Actor, ActorRuntime, RunnerKind, RuntimeStateSource};
use cccc_core::HomeLayout;
use cccc_runtime::{ActivityInstant, ActivitySnapshot};
use std::time::{Duration, SystemTime};

#[test]
fn claude_state_comes_only_from_its_managed_session() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = HomeLayout::from_path(temp.path()).expect("home");
    let group_id = "g_claude_projection";
    let mut actor = Actor::new("peer1");
    actor.runtime = ActorRuntime::Claude;
    actor.runtime_state_source = RuntimeStateSource::ManagedSession;

    let state = fields(&home, &actor, group_id, true, "pty");
    assert_eq!(state["effective_working_state"], "waiting");
    assert_eq!(
        state["effective_working_reason"],
        "managed_agent_session_pending"
    );
}

#[test]
fn pending_managed_session_and_structured_runtime_have_distinct_states() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = HomeLayout::from_path(temp.path()).expect("home");
    let mut claude = Actor::new("peer1");
    claude.runtime = ActorRuntime::Claude;
    let state = fields(&home, &claude, "g_test", true, "pty");
    assert_eq!(state["effective_working_state"], "waiting");
    assert_eq!(
        state["effective_working_reason"],
        "managed_agent_session_pending"
    );

    let mut custom = Actor::new("peer1");
    custom.runtime = ActorRuntime::Custom;
    custom.runner = RunnerKind::Headless;
    let state = fields(&home, &custom, "g_test", true, "headless");
    assert_eq!(state["effective_working_state"], "idle");
    assert_eq!(state["effective_working_reason"], "headless_running");
}

fn at(age_secs: u64) -> ActivityInstant {
    ActivityInstant {
        at: SystemTime::now() - Duration::from_secs(age_secs),
        age: Duration::from_secs(age_secs),
    }
}

/// Output of `bytes_per_second` in each of the given seconds (ages).
fn output(ages: impl IntoIterator<Item = u64>, bytes_per_second: u64) -> ActivitySnapshot {
    let recent_output = ages
        .into_iter()
        .map(|age| (Duration::from_secs(age), bytes_per_second))
        .collect::<Vec<_>>();
    ActivitySnapshot {
        last_output: recent_output.first().map(|(age, _)| at(age.as_secs())),
        output_bytes_since_input: recent_output.iter().map(|(_, bytes)| bytes).sum(),
        recent_output,
        ..ActivitySnapshot::default()
    }
}

#[test]
fn pty_without_observed_output_stays_honestly_unknown() {
    let config = PtyLivenessConfig::default();
    for activity in [None, Some(ActivitySnapshot::default())] {
        let liveness = pty_liveness(activity.as_ref(), config);
        assert_eq!(liveness.state, "waiting");
        assert_eq!(liveness.reason, "pty_running_state_unknown");
        assert_eq!(liveness.idle_seconds, None);
        assert_eq!(liveness.updated_at, None);
    }
}

#[test]
fn pty_moves_from_unknown_to_active_to_quiet() {
    let config = PtyLivenessConfig::default();
    assert_eq!(
        pty_liveness(None, config).reason,
        "pty_running_state_unknown"
    );

    // A busy TUI streaming ~4 KB/s for the last few seconds.
    let busy = output(0..5, 4096);
    let liveness = pty_liveness(Some(&busy), config);
    assert_eq!(liveness.state, "working");
    assert_eq!(liveness.reason, "pty_output_active");
    assert_eq!(liveness.idle_seconds, Some(0));
    let updated_at = liveness.updated_at.expect("updated_at");
    assert!(chrono::DateTime::parse_from_rfc3339(&updated_at).is_ok());
    assert!(updated_at.ends_with('Z'), "{updated_at}");

    // The same burst, now older than the active window.
    let quiet = output(45..50, 4096);
    let liveness = pty_liveness(Some(&quiet), config);
    assert_eq!(liveness.state, "idle");
    assert_eq!(liveness.reason, "pty_output_quiet");
    assert_eq!(liveness.idle_seconds, Some(45));
    assert!(liveness.updated_at.is_some());
}

#[test]
fn pty_redraw_noise_stays_idle() {
    let config = PtyLivenessConfig::default();
    // A status clock / cursor blink repainting ~40 bytes every second forever.
    let noise = output(0..=30, 40);
    let liveness = pty_liveness(Some(&noise), config);
    assert_eq!(liveness.state, "idle");
    assert_eq!(liveness.reason, "pty_output_quiet");
    assert_eq!(liveness.idle_seconds, Some(0));
}

#[test]
fn pty_small_output_answering_a_delivery_is_active_only_within_the_window() {
    let config = PtyLivenessConfig::default();
    let mut answered = output([1], 60);
    answered.last_input = Some(at(2));
    let liveness = pty_liveness(Some(&answered), config);
    assert_eq!(liveness.state, "working");
    assert_eq!(liveness.reason, "pty_output_active");

    // Delivered but nothing has come back since: not evidence of work.
    answered.output_bytes_since_input = 0;
    assert_eq!(pty_liveness(Some(&answered), config).state, "idle");

    // A delivery from long ago no longer vouches for trickling output.
    let mut stale = output([1], 60);
    stale.last_input = Some(at(40));
    assert_eq!(pty_liveness(Some(&stale), config).state, "idle");
}

#[test]
fn pty_liveness_honours_configured_window_and_threshold() {
    let config = PtyLivenessConfig {
        window: Duration::from_secs(10),
        min_bytes: 100,
    };
    assert_eq!(
        pty_liveness(Some(&output(0..3, 50)), config).state,
        "working"
    );
    assert_eq!(
        pty_liveness(Some(&output([12], 5000)), config).state,
        "idle"
    );
}

#[test]
fn running_pty_actor_without_a_session_reports_unknown_with_null_idle() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = HomeLayout::from_path(temp.path()).expect("home");
    let mut actor = Actor::new("peer1");
    actor.runtime = ActorRuntime::Custom;
    let state = fields(&home, &actor, "g_pty_no_session", true, "pty");
    assert_eq!(state["effective_working_state"], "waiting");
    assert_eq!(
        state["effective_working_reason"],
        "pty_running_state_unknown"
    );
    assert!(state["idle_seconds"].is_null());
    assert!(state["effective_working_updated_at"].is_null());
}
