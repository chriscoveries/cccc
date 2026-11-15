//! cwf#853 self-heal: a bound ChatGPT conversation that reached its maximum
//! length is moved to a fresh chat in the same tab and re-bound, so the lane
//! recovers without an operator.
//!
//! Safety rules:
//! - Only from the delivery worker's pre-claim path: nothing has been claimed
//!   for the full conversation, so no delivered message is replayed.
//! - Only with an empty composer and no running response (a draft is never
//!   discarded; a live turn is never interrupted).
//! - At most one rotation attempt per Actor per [`ROTATION_COOLDOWN`].
//! - The old binding stays in force until the new pairing is confirmed
//!   (`replace: true`, cwf#821), so a failed attempt changes nothing.
//! - Opt out with `CCCC_WEB_MODEL_AUTO_NEW_CHAT=0`.
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::AppState;

pub(super) const ROTATION_COOLDOWN: Duration = Duration::from_secs(15 * 60);
const ENV_SWITCH: &str = "CCCC_WEB_MODEL_AUTO_NEW_CHAT";

static LAST_ATTEMPT: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();

/// Automatic rotation is on unless the operator set the switch to an
/// explicit "off" value.
pub(super) fn enabled_from(value: Option<&str>) -> bool {
    !matches!(
        value.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("0" | "false" | "off" | "no")
    )
}

/// Pure decision: may the worker rotate this full conversation now?
pub(super) fn should_rotate(
    enabled: bool,
    readiness: &Value,
    target: &Value,
    last_attempt: Option<Instant>,
    now: Instant,
) -> bool {
    enabled
        && readiness["state"] == crate::browser_surface::CONVERSATION_LENGTH_EXCEEDED_STATE
        && target["kind"] == "existing_chat"
        && readiness["composer_chars"].as_u64().unwrap_or(0) == 0
        && readiness["composer_has_attachments"] != true
        && readiness["running"] != true
        && last_attempt.is_none_or(|at| now.saturating_duration_since(at) >= ROTATION_COOLDOWN)
}

/// Called by the delivery worker while it holds the Actor's browser work.
/// Books the attempt, then starts the re-pair on its own task: pairing needs
/// the same control permit, which the worker releases between polls.
pub(super) fn maybe_rotate(
    state: &AppState,
    group_id: &str,
    actor_id: &str,
    readiness: &Value,
    target: &Value,
) -> bool {
    let key = super::web_model_browser::key(group_id, actor_id);
    let now = Instant::now();
    let enabled = enabled_from(std::env::var(ENV_SWITCH).ok().as_deref());
    {
        let mut attempts = LAST_ATTEMPT
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if !should_rotate(enabled, readiness, target, attempts.get(&key).copied(), now) {
            return false;
        }
        attempts.insert(key, now);
    }
    tracing::warn!(
        group_id,
        actor_id,
        "Web-model conversation is full; opening a fresh chat in the same tab and re-pairing"
    );
    let state = state.clone();
    let group_id = group_id.to_owned();
    let actor_id = actor_id.to_owned();
    tokio::spawn(async move {
        // Let the worker drop its browser-work permit first.
        for attempt in 0..10 {
            tokio::time::sleep(Duration::from_secs(if attempt == 0 { 1 } else { 3 })).await;
            match super::web_model_pairing::start_replacement(&state, &group_id, &actor_id).await {
                Ok(_) => return,
                Err(error) if error.to_string().contains("busy") => continue,
                Err(error) => {
                    tracing::warn!(group_id, actor_id, %error, "Web-model fresh-chat re-pair could not start");
                    return;
                }
            }
        }
    });
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn full(composer: u64, running: bool) -> Value {
        json!({"state":"conversation_length_exceeded","composer_chars":composer,"running":running})
    }

    #[test]
    fn switch_defaults_on_and_accepts_explicit_off() {
        assert!(enabled_from(None));
        assert!(enabled_from(Some("1")));
        assert!(!enabled_from(Some("0")));
        assert!(!enabled_from(Some(" OFF ")));
        assert!(!enabled_from(Some("false")));
    }

    #[test]
    fn rotates_only_a_full_idle_existing_chat_once_per_cooldown() {
        let now = Instant::now();
        let chat = json!({"kind":"existing_chat"});
        assert!(should_rotate(true, &full(0, false), &chat, None, now));
        // Disabled, draft present, response running, or not full: never.
        assert!(!should_rotate(false, &full(0, false), &chat, None, now));
        assert!(!should_rotate(true, &full(12, false), &chat, None, now));
        assert!(!should_rotate(true, &full(0, true), &chat, None, now));
        assert!(!should_rotate(
            true,
            &json!({"state":"ready","composer_chars":0}),
            &chat,
            None,
            now
        ));
        assert!(!should_rotate(
            true,
            &json!({"state":"conversation_length_exceeded","composer_chars":0,"composer_has_attachments":true}),
            &chat,
            None,
            now
        ));
        // A pending new-chat bind is not an existing chat to rotate.
        assert!(!should_rotate(
            true,
            &full(0, false),
            &json!({"kind":"new_chat"}),
            None,
            now
        ));
        // Cooldown: no loop.
        let later = now + Duration::from_secs(60);
        assert!(!should_rotate(true, &full(0, false), &chat, Some(now), later));
        let after = now + ROTATION_COOLDOWN;
        assert!(should_rotate(true, &full(0, false), &chat, Some(now), after));
    }
}
