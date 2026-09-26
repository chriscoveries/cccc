use cccc_contracts::Actor;
use cccc_core::HomeLayout;
use serde_json::{Map, Value, json};
use std::sync::OnceLock;
use std::time::Duration;

/// Output this recent keeps a terminal (PTY) actor "working".
/// Override with `CCCC_PTY_ACTIVE_WINDOW_SECONDS` (5..=300).
pub(crate) const PTY_ACTIVE_WINDOW: Duration = Duration::from_secs(30);
/// Output volume within the window that counts as substantive on its own.
/// Override with `CCCC_PTY_ACTIVE_MIN_BYTES` (1..=1048576).
pub(crate) const PTY_SUBSTANTIVE_OUTPUT_BYTES: u64 = 2048;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PtyLivenessConfig {
    pub window: Duration,
    pub min_bytes: u64,
}

impl Default for PtyLivenessConfig {
    fn default() -> Self {
        Self {
            window: PTY_ACTIVE_WINDOW,
            min_bytes: PTY_SUBSTANTIVE_OUTPUT_BYTES,
        }
    }
}

fn pty_liveness_config() -> PtyLivenessConfig {
    static CONFIG: OnceLock<PtyLivenessConfig> = OnceLock::new();
    *CONFIG.get_or_init(|| {
        let env = |name: &str| {
            std::env::var(name)
                .ok()
                .and_then(|value| value.trim().parse::<u64>().ok())
        };
        let defaults = PtyLivenessConfig::default();
        PtyLivenessConfig {
            window: env("CCCC_PTY_ACTIVE_WINDOW_SECONDS")
                .map(|seconds| Duration::from_secs(seconds.clamp(5, 300)))
                .unwrap_or(defaults.window),
            min_bytes: env("CCCC_PTY_ACTIVE_MIN_BYTES")
                .map(|bytes| bytes.clamp(1, 1 << 20))
                .unwrap_or(defaults.min_bytes),
        }
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PtyLiveness {
    pub state: &'static str,
    pub reason: &'static str,
    pub idle_seconds: Option<u64>,
    pub updated_at: Option<String>,
}

/// Busy/idle for a plain PTY actor, from bytes the daemon read off its PTY.
///
/// Heuristic: output within `window` is "substantive" when either
/// - at least `min_bytes` arrived within the window, or
/// - it followed an input delivery that itself happened within the window
///   (the TUI reacting to a message we just submitted).
///
/// The byte floor keeps idle redraw noise (cursor blink, status clocks, a
/// spinner glyph repainted in place) quiet: those emit tens of bytes per
/// second at most, whereas live samples of busy devin/hermes TUIs emitted
/// roughly 1.5-12 KB/s and idle hermes TUIs emitted nothing at all. A single
/// resize/attach repaint can still read as active for one window.
pub(crate) fn pty_liveness(
    activity: Option<&cccc_runtime::ActivitySnapshot>,
    config: PtyLivenessConfig,
) -> PtyLiveness {
    let Some((activity, last_output)) =
        activity.and_then(|activity| activity.last_output.map(|last| (activity, last)))
    else {
        return PtyLiveness {
            state: "waiting",
            reason: "pty_running_state_unknown",
            idle_seconds: None,
            updated_at: None,
        };
    };
    let recent = last_output.age <= config.window;
    let voluminous = activity.output_bytes_within(config.window) >= config.min_bytes;
    let answers_delivery = activity
        .last_input
        .is_some_and(|input| input.age <= config.window)
        && activity.output_bytes_since_input > 0;
    let (state, reason) = if recent && (voluminous || answers_delivery) {
        ("working", "pty_output_active")
    } else {
        ("idle", "pty_output_quiet")
    };
    PtyLiveness {
        state,
        reason,
        idle_seconds: Some(last_output.age.as_secs()),
        updated_at: Some(
            chrono::DateTime::<chrono::Utc>::from(last_output.at)
                .to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
        ),
    }
}

pub fn runtime_actor_fields(
    home: &HomeLayout,
    actor: &Actor,
    group_id: &str,
    running: bool,
) -> Map<String, Value> {
    let runner_effective = if super::actor_runtime::is_structured(actor) {
        "headless"
    } else {
        "pty"
    };
    fields(home, actor, group_id, running, runner_effective)
}

pub(super) fn fields(
    _home: &HomeLayout,
    actor: &Actor,
    group_id: &str,
    running: bool,
    runner_effective: &str,
) -> Map<String, Value> {
    let managed_session = super::local_headless::running(group_id, &actor.id)
        || super::local_headless::uses_managed_session(actor);
    let local_state = running
        .then(|| super::local_headless::status(group_id, &actor.id))
        .flatten();
    let mut idle_seconds = None;
    let (state, reason, updated_at, active_task_id) = if !running {
        (
            "stopped".to_owned(),
            "runner_not_running".to_owned(),
            None,
            None,
        )
    } else if let Some(local_state) = local_state {
        (
            local_state.status,
            if runner_effective == "pty" {
                "managed_agent_session".to_owned()
            } else {
                "provider_headless_session".to_owned()
            },
            Some(local_state.updated_at),
            local_state.task_id,
        )
    } else if managed_session {
        (
            "waiting".to_owned(),
            "managed_agent_session_pending".to_owned(),
            None,
            None,
        )
    } else if runner_effective == "headless" {
        ("idle".to_owned(), "headless_running".to_owned(), None, None)
    } else {
        let activity = cccc_runtime::activity(group_id, &actor.id).ok();
        let liveness = pty_liveness(activity.as_ref(), pty_liveness_config());
        idle_seconds = liveness.idle_seconds;
        (
            liveness.state.to_owned(),
            liveness.reason.to_owned(),
            liveness.updated_at,
            None,
        )
    };

    Map::from_iter([
        ("idle_seconds".into(), json!(idle_seconds)),
        ("runner_effective".into(), json!(runner_effective)),
        ("effective_working_state".into(), json!(state)),
        ("effective_working_reason".into(), json!(reason)),
        ("effective_working_updated_at".into(), json!(updated_at)),
        ("effective_active_task_id".into(), json!(active_task_id)),
    ])
}
