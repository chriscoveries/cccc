use super::working_state::{PtyLivenessConfig, fields, pty_liveness};
use cccc_contracts::{
    Actor, ActorRuntime, DaemonRequest, RunnerKind, RuntimeMode, RuntimeStateSource,
};
use cccc_core::{GroupStore, HomeLayout};
use cccc_runtime::{ActivityInstant, ActivitySnapshot};
use serde_json::Map;
use std::time::{Duration, SystemTime};

#[test]
fn stopped_actor_listing_uses_the_configured_surface_without_starting_work() {
    let temp = tempfile::tempdir().expect("fixture directory");
    let home = HomeLayout::from_path(temp.path()).expect("fixture home");
    home.initialize().expect("initialize fixture home");
    let store = GroupStore::new(home.clone()).expect("fixture store");
    let mut group = store.create("Stopped surfaces", "").expect("fixture group");
    let cases = [
        (ActorRuntime::Pi, RuntimeMode::Default, "headless"),
        (ActorRuntime::Antigravity, RuntimeMode::Acp, "headless"),
        (ActorRuntime::Auggie, RuntimeMode::Acp, "headless"),
        (ActorRuntime::Copilot, RuntimeMode::Acp, "headless"),
        (ActorRuntime::Cursor, RuntimeMode::Acp, "headless"),
        (ActorRuntime::Devin, RuntimeMode::Acp, "headless"),
        (ActorRuntime::Kimi, RuntimeMode::Acp, "headless"),
        (ActorRuntime::Kiro, RuntimeMode::Acp, "headless"),
        (ActorRuntime::Hermes, RuntimeMode::Acp, "headless"),
        (ActorRuntime::Droid, RuntimeMode::Acp, "headless"),
        (ActorRuntime::Kiro, RuntimeMode::Default, "pty"),
        (ActorRuntime::Hermes, RuntimeMode::Default, "pty"),
        (ActorRuntime::Droid, RuntimeMode::Default, "pty"),
        (ActorRuntime::Kimi, RuntimeMode::Default, "pty"),
        (ActorRuntime::Auggie, RuntimeMode::Default, "pty"),
        (ActorRuntime::Claude, RuntimeMode::Default, "pty"),
        (ActorRuntime::Codex, RuntimeMode::Default, "pty"),
        (ActorRuntime::Deepseek, RuntimeMode::Default, "headless"),
        (ActorRuntime::WebModel, RuntimeMode::Default, "headless"),
    ];
    for (index, (runtime, mode, _)) in cases.iter().enumerate() {
        let mut actor = Actor::new(format!("stopped-{index}"));
        actor.runtime = *runtime;
        actor.runtime_mode = *mode;
        group.actors.push(actor);
    }
    store.save(&group).expect("save stopped actors");
    let request = DaemonRequest {
        v: 1,
        op: "actor_list".into(),
        args: Map::new(),
    };
    let values = super::actor_listing::list(&home, &group, &request)
        .expect("list configured stopped surfaces");
    assert_eq!(values.len(), cases.len());
    for (value, (_, _, runner)) in values.iter().zip(cases) {
        assert_eq!(value["runner"], runner, "{}", value["id"]);
        assert_eq!(value["runner_effective"], runner, "{}", value["id"]);
        assert_eq!(value["running"], false);
        assert_eq!(value["pid"], serde_json::Value::Null);
        assert_eq!(value["effective_working_state"], "stopped");
        assert!(
            super::local_headless::structured_state(
                &group.group_id,
                value["id"].as_str().expect("actor ID")
            )
            .is_none()
        );
    }
}

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

/// The volume comes from a real recorded PTY stream (claw stage, Hermes
/// startup — `crates/cccc-runtime/tests/fixtures/PROVENANCE.md`): 7340 bytes of
/// actual TUI traffic arriving inside one second. The ages are the control —
/// a byte stream records content, not time — so this pins the classification
/// at the recorded volume rather than at a number chosen to suit the floor.
#[test]
fn recorded_hermes_startup_reads_as_working_then_idle_once_quiet() {
    let captured = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../cccc-runtime/tests/fixtures/hermes-pty-startup-claw-stage.pty"
    ));
    let bytes = captured.len() as u64;
    assert_eq!(bytes, 7340, "the recorded capture drifted");
    let config = PtyLivenessConfig::default();

    let mut startup = output([0], bytes);
    startup.last_output = Some(at(0));
    let liveness = pty_liveness(Some(&startup), config);
    assert_eq!(
        liveness.state, "working",
        "a real startup burst is output, not redraw noise"
    );
    assert_eq!(liveness.reason, "pty_output_active");

    let mut quiet = output([90], bytes);
    quiet.last_output = Some(at(90));
    let liveness = pty_liveness(Some(&quiet), config);
    assert_eq!(liveness.state, "idle");
    assert_eq!(liveness.reason, "pty_output_quiet");
    assert_eq!(liveness.idle_seconds, Some(90));
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
