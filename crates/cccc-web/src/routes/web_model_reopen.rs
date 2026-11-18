//! cwf#821 / cwf#825 self-heal: a web-model tab that no longer shows its bound
//! conversation ("Saved chat unavailable", `existing_chat_unavailable`, or
//! "Could not load this conversation" on another URL, often after a ChatGPT
//! 429) is navigated back to the bound URL by the delivery worker.
//!
//! Never a loop: at most one reopen per Actor per [`REOPEN_COOLDOWN`], never
//! while the load-error backoff (which honours Retry-After) says wait, and
//! never over sign-in, a security check, a page still loading or a full
//! conversation (that one rotates to a fresh chat instead, cwf#853). A failed
//! reopen leaves the worker running instead of stopping it, so the lane
//! retries on the next window rather than staying dead until a restart.
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use cccc_core::{GroupDoc, GroupStore};
use serde_json::{Value, json};

use super::web_model_delivery_state::update_target;
use crate::{
    AppState,
    api::ApiError,
    browser_surface::{LoadRecovery, is_load_error_state},
};

const IDLE_CLOSE_ENV: &str = "CCCC_WEB_MODEL_IDLE_CLOSE_SECS";

fn group_for(state: &AppState, group: &str) -> Result<GroupDoc, ApiError> {
    GroupStore::new(state.home.clone())
        .and_then(|store| store.load(group))
        .map_err(super::web_model_connector_store::io_error)
}

fn active_turn<'a>(group: &'a GroupDoc, actor: &str) -> Option<&'a str> {
    group
        .extra
        .get("runtime_states")
        .and_then(|states| states[actor]["active_turn_id"].as_str())
        .filter(|id| !id.is_empty())
}

pub(super) fn unresolved(target: &Value) -> bool {
    matches!(
        target["last_delivery_status"].as_str(),
        Some(
            "submitting"
                | "deferred"
                | "preflight_deferred"
                | "submission_ambiguous"
                | "legacy_recovery_submitting"
                | "legacy_submission_unverified"
                | "pending_new_chat_bind"
                | "ambiguous"
                | "completion_ambiguous"
                | "submission_ambiguous_completion_pending"
                | "completion_conflict"
        )
    )
}

fn pending_work(state: &AppState, group: &GroupDoc, actor: &str) -> Result<bool, ApiError> {
    cccc_core::inbox::pending_deliveries(&state.home, group, actor, 1)
        .map(|events| !events.is_empty())
        .map_err(super::web_model_connector_store::io_error)
}

pub(super) fn has_demand(state: &AppState, group: &str, actor: &str, target: &Value) -> bool {
    group_for(state, group).is_ok_and(|doc| {
        unresolved(target)
            || active_turn(&doc, actor).is_some()
            || pending_work(state, &doc, actor).unwrap_or(false)
    })
}

pub(super) fn may_replace_timed_out_page(
    state: &AppState,
    group: &str,
    actor: &str,
    target: &Value,
) -> bool {
    group_for(state, group).is_ok_and(|doc| {
        let active = active_turn(&doc, actor);
        let unstarted = target["last_delivery_status"] == "preflight_deferred"
            && target["last_submission_evidence"]["submission_evidence"] == "send_not_started"
            && active.is_some_and(|id| target["last_delivery_turn_id"] == id);
        unstarted
            || (!unresolved(target)
                && active.is_none()
                && pending_work(state, &doc, actor).unwrap_or(false))
    })
}

fn idle_close_after(value: Option<&str>) -> Option<Duration> {
    value?
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
}

pub(super) async fn consider_idle_close(
    state: &AppState,
    group: &str,
    actor: &str,
    surface: &Value,
    target: &Value,
    eligible: bool,
) -> Result<(), ApiError> {
    let Some(after) = idle_close_after(std::env::var(IDLE_CLOSE_ENV).ok().as_deref()) else {
        return Ok(());
    };
    let Some(target_id) = surface["target_id"].as_str() else {
        return Ok(());
    };
    let eligible = if eligible && target["kind"] == "existing_chat" && !unresolved(target) {
        let doc = group_for(state, group)?;
        let pending_pair = super::web_model_pairing::actor_pairing(state, group, actor)?
            .is_some_and(|pair| {
                matches!(
                    pair["state"].as_str(),
                    Some("waiting" | "awaiting_confirmation")
                ) && pair["expires_at_ms"]
                    .as_i64()
                    .is_none_or(|ms| ms > chrono::Utc::now().timestamp_millis())
            });
        active_turn(&doc, actor).is_none() && !pending_pair && !pending_work(state, &doc, actor)?
    } else {
        false
    };
    state
        .browser_surfaces
        .consider_actor_idle_close(
            &super::web_model_browser::key(group, actor),
            target_id,
            eligible,
            after,
        )
        .await
        .map(|_| ())
        .map_err(|error| ApiError::bad(error.to_string()))
}

pub(super) fn load_recovery_blocks_open(target: &Value) -> bool {
    let record = &target["load_recovery"];
    record.is_object()
        && record["next_reload_at_ms"]
            .as_i64()
            .is_none_or(|deadline| deadline > chrono::Utc::now().timestamp_millis())
}

pub(super) fn book_recovery_open(
    state: &AppState,
    group: &str,
    actor: &str,
    target: &Value,
) -> Result<(), ApiError> {
    let record = &target["load_recovery"];
    if record.is_null() {
        return Ok(());
    }
    let mut record: LoadRecovery =
        serde_json::from_value(record.clone()).map_err(|error| ApiError::bad(error.to_string()))?;
    if !record.book(chrono::Utc::now().timestamp_millis()) {
        return Err(ApiError::bad("Conversation recovery retry is not due"));
    }
    update_target(state, group, actor, json!({"load_recovery":record}))
}

pub(super) async fn handle_load_error(
    state: &AppState,
    group: &str,
    actor: &str,
    target: &Value,
    readiness: &Value,
) -> Result<bool, ApiError> {
    let session_key = super::web_model_browser::key(group, actor);
    let page_state = readiness["state"].as_str().unwrap_or_default();
    if !is_load_error_state(page_state) {
        if readiness["ready"] == true
            && saved_page_matches(target, readiness)
            && (target["kind"] != "existing_chat" || readiness["thread_loaded"] == true)
            && target["load_recovery"].is_object()
        {
            update_target(state, group, actor, json!({"load_recovery":null}))?;
        }
        return Ok(false);
    }

    let now = chrono::Utc::now().timestamp_millis();
    let hint = state
        .browser_surfaces
        .take_initial_retry_after(&session_key)
        .await;
    let previous = serde_json::from_value::<LoadRecovery>(target["load_recovery"].clone()).ok();
    let mut record = previous
        .clone()
        .unwrap_or_else(|| LoadRecovery::new(page_state, now, hint));
    if let Some(hint) = hint {
        record.apply_retry_after(now, hint);
    }
    // Error pages can still expose an attachment or a running reply even
    // when they have no usable composer. Keep the episode without navigating.
    let busy = matches!(
        target["last_delivery_status"].as_str(),
        Some("deferred" | "submitting" | "legacy_recovery_submitting")
    ) || super::web_model_reconcile::composer_occupied(readiness)
        || readiness["running"] == true;
    let reload = !busy && record.book(now);
    if previous.as_ref() != Some(&record) {
        update_target(state, group, actor, json!({"load_recovery":record}))?;
    }
    if reload {
        state
            .browser_surfaces
            .reload_conversation(
                &session_key,
                target["url"].as_str().unwrap_or_default(),
                &mut record,
            )
            .await;
        update_target(state, group, actor, json!({"load_recovery":record}))?;
        tracing::info!(
            group,
            actor,
            attempts = record.reload_attempts,
            state = record.state,
            "Checked managed conversation after a bounded recovery reload"
        );
    }
    Ok(true)
}

pub(super) fn saved_page_matches(target: &Value, readiness: &Value) -> bool {
    let expected = target["url"].as_str().unwrap_or_default();
    let observed = readiness["tab_url"].as_str().unwrap_or_default();
    if crate::browser_surface::is_chatgpt_url(expected) {
        crate::browser_surface::conversation_target_matches(expected, observed)
    } else {
        !expected.is_empty() && expected == observed
    }
}

pub(super) const REOPEN_COOLDOWN: Duration = Duration::from_secs(10 * 60);

static LAST_REOPEN: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();

/// Page states a reopen may navigate away from.
pub(super) fn state_allows_reopen(state: &str) -> bool {
    !matches!(
        state,
        "loading" | "sign_in_required" | "verification_required" | "conversation_length_exceeded"
    )
}

/// Does the load-error backoff (Retry-After aware) still say wait?
pub(super) fn backoff_says_wait(rate_limit: Option<&Value>, now_ms: i64) -> bool {
    rate_limit
        .and_then(|record| record.get("next_reload_at_ms"))
        .is_some_and(|deadline| deadline.as_i64().is_none_or(|next| next > now_ms))
}

/// Pure decision for one Actor.
pub(super) fn reopen_due(
    last: Option<Instant>,
    now: Instant,
    rate_limit: Option<&Value>,
    now_ms: i64,
) -> bool {
    last.is_none_or(|at| now.saturating_duration_since(at) >= REOPEN_COOLDOWN)
        && !backoff_says_wait(rate_limit, now_ms)
}

/// Books a reopen for this Actor if one is due. Booking happens before the
/// navigation, so an attempt that fails still waits a full window.
pub(super) fn try_book(key: &str, rate_limit: Option<&Value>) -> bool {
    let now = Instant::now();
    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut map = LAST_REOPEN
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    if !reopen_due(map.get(key).copied(), now, rate_limit, now_ms) {
        return false;
    }
    map.insert(key.to_owned(), now);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn idle_close_requires_an_explicit_positive_timeout() {
        assert_eq!(idle_close_after(None), None);
        assert_eq!(idle_close_after(Some("0")), None);
        assert_eq!(idle_close_after(Some("false")), None);
        assert_eq!(idle_close_after(Some("60")), Some(Duration::from_secs(60)));
        assert!(unresolved(
            &json!({"last_delivery_status":"preflight_deferred"})
        ));
    }

    #[test]
    fn never_navigates_over_sign_in_security_loading_or_a_full_chat() {
        for state in [
            "loading",
            "sign_in_required",
            "verification_required",
            "conversation_length_exceeded",
        ] {
            assert!(!state_allows_reopen(state), "{state}");
        }
        for state in [
            "page_not_ready",
            "ready",
            "rate_limited",
            "conversation_load_failed",
        ] {
            assert!(state_allows_reopen(state), "{state}");
        }
    }

    #[test]
    fn at_most_one_reopen_per_ten_minutes() {
        let now = Instant::now();
        assert!(reopen_due(None, now, None, 0));
        assert!(!reopen_due(
            Some(now),
            now + Duration::from_secs(60),
            None,
            0
        ));
        assert!(!reopen_due(
            Some(now),
            now + REOPEN_COOLDOWN - Duration::from_secs(1),
            None,
            0
        ));
        assert!(reopen_due(Some(now), now + REOPEN_COOLDOWN, None, 0));
    }

    #[test]
    fn honours_the_retry_after_backoff_window() {
        let now = Instant::now();
        let waiting = json!({"next_reload_at_ms": 10_000});
        assert!(!reopen_due(None, now, Some(&waiting), 9_000));
        assert!(reopen_due(None, now, Some(&waiting), 10_000));
        assert!(reopen_due(None, now, Some(&json!({})), 9_000));
    }

    #[test]
    fn booking_is_per_actor_and_blocks_a_second_attempt() {
        let a = format!("test-reopen-a-{:?}", Instant::now());
        let b = format!("test-reopen-b-{:?}", Instant::now());
        assert!(try_book(&a, None));
        assert!(!try_book(&a, None), "second attempt inside the window");
        assert!(try_book(&b, None), "another Actor is independent");
    }
}
