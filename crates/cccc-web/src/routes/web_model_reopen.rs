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

use serde_json::Value;

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
        .and_then(|r| r["next_reload_at_ms"].as_i64())
        .is_some_and(|next| next > now_ms)
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
