//! cwf#825 self-heal for an ambiguous ("delivery busy") web-model lane.
//!
//! An ambiguous browser submission pauses delivery until someone reviews the
//! conversation and resumes it. Operators were doing that review by script
//! (resume-delivery, then remove) dozens of times a day; a lane left alone
//! stayed paused for many hours and `pairing remove` was refused with
//! `delivery_unresolved` or "delivery is busy".
//!
//! This module holds the pure decisions. The checks that make a resume safe
//! are the same ones the manual resume-delivery route applies (the tab shows
//! the saved conversation, it is ready, its composer is empty and no response
//! is running); nothing is ever replayed.
use crate::{AppState, api::ApiError};
use serde_json::{Value, json};
use std::time::Duration;

/// Env switch for the worker's automatic resume, in minutes. Unset or 0 = off.
pub(super) const ENV_AUTO_RESUME_MIN: &str = "CCCC_WEB_MODEL_AUTO_RESUME_AMBIGUOUS_MIN";
/// How long a remove waits for the delivery worker's poll to release the
/// Actor's control permit before refusing as busy.
pub(super) const REMOVE_BUSY_WAIT: Duration = Duration::from_secs(30);

/// A remove (or the worker) may try to reconcile this target first.
pub(super) fn reconcilable(target: &Value) -> Option<String> {
    (target["kind"] == "existing_chat" && target["last_delivery_status"] == "submission_ambiguous")
        .then(|| target["last_delivery_id"].as_str().unwrap_or(""))
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

/// Minutes after which the worker resumes an ambiguous delivery by itself.
pub(super) fn auto_resume_after(value: Option<&str>) -> Option<Duration> {
    value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|m| *m > 0)
        .map(|m| Duration::from_secs(m.min(24 * 60) * 60))
}

// cwf#916: durable bounded repair state, kept on the delivery target record
// so attempts survive restarts and worker races. `scheduled` retries with
// exponential backoff, `review_required` stops auto-repair when the blocker
// needs a human (an unprovable draft), `exhausted` stops it at the cap.
pub(super) const REPAIR_MAX_ATTEMPTS: u32 = 8;
const REPAIR_MAX_BACKOFF_MS: i64 = 4 * 60 * 60 * 1000;

/// Spacing before the retry after `attempts` recorded attempts:
/// base * 2^(attempts-1), capped. Attempt 1 waits `base`.
pub(super) fn repair_backoff_ms(attempts: u32, base_ms: i64) -> i64 {
    let shift = attempts.saturating_sub(1).min(20);
    base_ms
        .saturating_mul(1i64 << shift)
        .min(REPAIR_MAX_BACKOFF_MS)
}

/// First sight of the wedge: record detection once, before any attempt.
pub(super) fn repair_detected(target: &Value) -> Value {
    json!({"repair":{
        "reason":"submission_ambiguous",
        "status":"scheduled",
        "attempts":0,
        "first_detected_at":target["last_delivery_at"],
        "last_attempt_at":Value::Null,
        "next_retry_at_ms":Value::Null,
        "last_result":Value::Null,
    }})
}

/// A delivery reviewed after it went ambiguous belongs to the reviewer.
fn reviewed_after_ambiguous(target: &Value) -> bool {
    let Some(at) = target["last_delivery_at"]
        .as_str()
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
    else {
        return false;
    };
    target["last_delivery_reviewed_at"]
        .as_str()
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .is_some_and(|reviewed| reviewed >= at)
}

/// An automatic repair attempt is due: the wedge must still stand, a human
/// review closes it for good, terminal states never retry, scheduled
/// retries wait for next_retry_at_ms, and the first attempt keeps the
/// operator's minimum-age gate.
pub(super) fn repair_due(
    target: &Value,
    after: Option<Duration>,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    if reconcilable(target).is_none() || reviewed_after_ambiguous(target) {
        return false;
    }
    let repair = &target["repair"];
    if matches!(
        repair["status"].as_str(),
        Some("review_required" | "exhausted")
    ) {
        return false;
    }
    if let Some(next) = repair["next_retry_at_ms"].as_i64() {
        return now.timestamp_millis() >= next;
    }
    auto_resume_due(target, after, now)
}

/// Record one attempt's outcome. An occupied composer the ownership proof
/// cannot clear is `review_required` — a human must look; anything else
/// retries on backoff until the cap.
pub(super) fn repair_note_attempt(
    target: &Value,
    result: &str,
    base: Duration,
    now: chrono::DateTime<chrono::Utc>,
) -> Value {
    let repair = &target["repair"];
    let attempts = repair["attempts"].as_u64().unwrap_or(0) as u32 + 1;
    let unsafe_draft = result == "composer_occupied";
    let status = if unsafe_draft {
        "review_required"
    } else if attempts >= REPAIR_MAX_ATTEMPTS {
        "exhausted"
    } else {
        "scheduled"
    };
    let now_ms = now.timestamp_millis();
    json!({"repair":{
        "reason":"submission_ambiguous",
        "status":status,
        "attempts":attempts,
        "first_detected_at":if repair["first_detected_at"].is_null() {
            target["last_delivery_at"].clone()
        } else {
            repair["first_detected_at"].clone()
        },
        "last_attempt_at":cccc_contracts::utc_now(),
        "next_retry_at_ms":(status == "scheduled")
            .then(|| now_ms + repair_backoff_ms(attempts, base.as_millis() as i64)),
        "last_result":result,
    }})
}

/// The wedge resolved; drop the repair record.
pub(super) fn repair_cleared() -> Value {
    json!({"repair":Value::Null})
}

/// Has this ambiguous delivery waited long enough for an automatic resume?
pub(super) fn auto_resume_due(
    target: &Value,
    after: Option<Duration>,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    let Some(after) = after else { return false };
    if reconcilable(target).is_none() {
        return false;
    }
    let Some(at) = target["last_delivery_at"]
        .as_str()
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
    else {
        return false;
    };
    if reviewed_after_ambiguous(target) {
        return false;
    }
    now.signed_duration_since(at.with_timezone(&chrono::Utc))
        .to_std()
        .is_ok_and(|age| age >= after)
}

/// The resume-delivery safety checks on a fresh readiness snapshot.
pub(super) fn tab_safe_to_resume(readiness: &Value, matches_saved_chat: bool) -> bool {
    matches_saved_chat
        && readiness["ready"] == true
        && readiness["composer_chars"].as_u64().unwrap_or(0) == 0
        && readiness["composer_has_attachments"] != true
        && readiness["running"] != true
}

pub(super) fn composer_occupied(readiness: &Value) -> bool {
    readiness["composer_chars"].as_u64().unwrap_or(0) > 0
        || readiness["composer_has_attachments"] == true
}

pub(super) fn occupied_error() -> ApiError {
    ApiError::bad_code(
        "composer_occupied",
        "The conversation has an unsent draft. Check the message box and try again.",
        json!({"reason":"composer_occupied","retryable":true}),
    )
}

/// New proof comes from a successful readback after CCCC inserted the text.
/// A continuation of the same attempt keeps its existing proof.
pub(super) fn staged_draft_proof(
    target: &Value,
    surface: &Value,
    evidence: &Value,
    delivery_id: &str,
) -> Value {
    let Some(digest) = evidence["baseline"]["staged_draft_sha256"].as_str() else {
        return if target["last_delivery_id"] == delivery_id {
            target["staged_draft"].clone()
        } else {
            Value::Null
        };
    };
    json!({
        "sha256":digest,"delivery_id":delivery_id,
        "binding_revision":target["binding_revision"],
        "actor_identity":surface["metadata"]["actor_identity"],
        "target_id":surface["target_id"],
    })
}

pub(super) async fn clear_owned_draft(
    state: &AppState,
    group: &str,
    actor: &str,
    target: &Value,
    readiness: &Value,
) -> Result<bool, ApiError> {
    if !composer_occupied(readiness)
        || readiness["composer_has_attachments"] == true
        || readiness["running"] != false
        || target["kind"] != "existing_chat"
        || target["last_delivery_status"] == "deferred"
        || !super::web_model_reopen::saved_page_matches(target, readiness)
    {
        return Ok(false);
    }
    let key = super::web_model_browser::key(group, actor);
    let surface = state.browser_surfaces.info(&key).await;
    let Some(target_id) = surface["target_id"].as_str() else {
        return Ok(false);
    };
    let proof = &target["staged_draft"];
    let delivery_hash = (proof["delivery_id"].is_string()
        && proof["delivery_id"] == target["last_delivery_id"]
        && proof["binding_revision"].is_string()
        && proof["binding_revision"] == target["binding_revision"]
        && proof["actor_identity"].is_string()
        && proof["actor_identity"] == surface["metadata"]["actor_identity"]
        && proof["target_id"] == target_id)
        .then(|| proof["sha256"].as_str())
        .flatten();

    let pairing = super::web_model_pairing::actor_pairing(state, group, actor)?;
    let pairing_hash = pairing
        .as_ref()
        .filter(|pair| {
            matches!(pair["state"].as_str(), Some("failed" | "cancelled"))
                || (matches!(
                    pair["state"].as_str(),
                    Some("waiting" | "awaiting_confirmation")
                ) && pair["expires_at_ms"]
                    .as_i64()
                    .is_some_and(|ms| ms <= chrono::Utc::now().timestamp_millis()))
        })
        .and_then(|pair| pair["code_hash"].as_str());
    state
        .browser_surfaces
        .clear_owned_actor_draft(
            &key,
            target_id,
            target["url"].as_str().unwrap_or_default(),
            delivery_hash,
            pairing_hash,
        )
        .await
        .map_err(|error| ApiError::bad(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ambiguous(at: &str) -> Value {
        json!({"kind":"existing_chat","last_delivery_status":"submission_ambiguous",
            "last_delivery_id":"d1","last_delivery_at":at})
    }

    #[test]
    fn only_an_ambiguous_existing_chat_delivery_is_reconcilable() {
        assert_eq!(reconcilable(&ambiguous("x")), Some("d1".into()));
        assert_eq!(
            reconcilable(
                &json!({"kind":"new_chat","last_delivery_status":"submission_ambiguous","last_delivery_id":"d1"})
            ),
            None
        );
        assert_eq!(
            reconcilable(
                &json!({"kind":"existing_chat","last_delivery_status":"submitted","last_delivery_id":"d1"})
            ),
            None
        );
        assert_eq!(
            reconcilable(
                &json!({"kind":"existing_chat","last_delivery_status":"submission_ambiguous"})
            ),
            None
        );
    }

    #[test]
    fn automatic_resume_is_opt_in_and_bounded() {
        assert_eq!(auto_resume_after(None), None);
        assert_eq!(auto_resume_after(Some("0")), None);
        assert_eq!(auto_resume_after(Some("abc")), None);
        assert_eq!(
            auto_resume_after(Some("20")),
            Some(Duration::from_secs(1200))
        );
        assert_eq!(
            auto_resume_after(Some("99999")),
            Some(Duration::from_secs(86400))
        );
    }

    #[test]
    fn automatic_resume_waits_and_respects_a_review() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-11-15T08:00:00Z")
            .expect("valid RFC 3339 time")
            .with_timezone(&chrono::Utc);
        let after = Some(Duration::from_secs(20 * 60));
        assert!(auto_resume_due(
            &ambiguous("2026-11-15T07:30:00Z"),
            after,
            now
        ));
        assert!(!auto_resume_due(
            &ambiguous("2026-11-15T07:50:00Z"),
            after,
            now
        ));
        assert!(!auto_resume_due(
            &ambiguous("2026-11-15T07:30:00Z"),
            None,
            now
        ));
        assert!(!auto_resume_due(&ambiguous("not a time"), after, now));
        let mut reviewed = ambiguous("2026-11-15T07:30:00Z");
        reviewed["last_delivery_reviewed_at"] = json!("2026-11-15T07:31:00Z");
        assert!(!auto_resume_due(&reviewed, after, now));
    }

    #[test]
    fn resume_needs_an_idle_ready_tab_on_the_saved_chat() {
        let ok = json!({"ready":true,"composer_chars":0,"running":false});
        assert!(tab_safe_to_resume(&ok, true));
        assert!(!tab_safe_to_resume(&ok, false));
        assert!(!tab_safe_to_resume(&json!({"ready":false}), true));
        assert!(!tab_safe_to_resume(
            &json!({"ready":true,"composer_chars":5}),
            true
        ));
        assert!(!tab_safe_to_resume(
            &json!({"ready":true,"running":true}),
            true
        ));
        assert!(!tab_safe_to_resume(
            &json!({"ready":true,"composer_has_attachments":true}),
            true
        ));
    }

    #[test]
    fn repair_backoff_is_exponential_and_capped() {
        let base = 60_000i64;
        assert_eq!(repair_backoff_ms(1, base), base);
        assert_eq!(repair_backoff_ms(2, base), 2 * base);
        assert_eq!(repair_backoff_ms(3, base), 4 * base);
        assert_eq!(repair_backoff_ms(20, base), REPAIR_MAX_BACKOFF_MS);
        assert_eq!(repair_backoff_ms(0, base), base);
    }

    #[test]
    fn repair_due_gates_on_recorded_state() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-11-15T08:00:00Z")
            .expect("valid RFC 3339 time")
            .with_timezone(&chrono::Utc);
        let after = Some(Duration::from_secs(20 * 60));
        let mut target = ambiguous("2026-11-15T07:30:00Z");
        // No record: the first attempt keeps the minimum-age gate.
        assert!(repair_due(&target, after, now));
        target["last_delivery_at"] = json!("2026-11-15T07:55:00Z");
        assert!(!repair_due(&target, after, now));
        // Detection alone does not make it due early.
        let mut detected = ambiguous("2026-11-15T07:30:00Z");
        detected["repair"] = repair_detected(&detected)["repair"].clone();
        assert!(repair_due(&detected, after, now));
        // A scheduled retry waits for its timestamp, then fires.
        let mut retry = ambiguous("2026-11-15T07:30:00Z");
        retry["repair"] = json!({"status":"scheduled","attempts":2,
            "next_retry_at_ms":now.timestamp_millis() + 60_000});
        assert!(!repair_due(&retry, after, now));
        retry["repair"]["next_retry_at_ms"] = json!(now.timestamp_millis() - 1);
        assert!(repair_due(&retry, after, now));
        // Terminal states and a human review never retry.
        for status in ["review_required", "exhausted"] {
            let mut terminal = ambiguous("2026-11-15T07:30:00Z");
            terminal["repair"] = json!({"status":status,"attempts":1,
                "next_retry_at_ms":0});
            assert!(!repair_due(&terminal, after, now));
        }
        let mut reviewed = ambiguous("2026-11-15T07:30:00Z");
        reviewed["repair"] = json!({"status":"scheduled","attempts":1,
            "next_retry_at_ms":0});
        reviewed["last_delivery_reviewed_at"] = json!("2026-11-15T07:31:00Z");
        assert!(!repair_due(&reviewed, after, now));
        // The wedge gone means the record is inert.
        let mut resolved = ambiguous("2026-11-15T07:30:00Z");
        resolved["last_delivery_status"] = json!("resolved");
        resolved["repair"] = json!({"status":"scheduled","attempts":1,
            "next_retry_at_ms":0});
        assert!(!repair_due(&resolved, after, now));
    }

    #[test]
    fn repair_attempts_record_backoff_and_stop() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-11-15T08:00:00Z")
            .expect("valid RFC 3339 time")
            .with_timezone(&chrono::Utc);
        let base = Duration::from_secs(20 * 60);
        let mut target = ambiguous("2026-11-15T07:30:00Z");
        target["repair"] = repair_detected(&target)["repair"].clone();
        let patch = repair_note_attempt(&target, "invalid_request", base, now);
        let repair = &patch["repair"];
        assert_eq!(repair["attempts"], 1);
        assert_eq!(repair["status"], "scheduled");
        assert_eq!(
            repair["next_retry_at_ms"].as_i64().unwrap() - now.timestamp_millis(),
            base.as_millis() as i64
        );
        assert_eq!(repair["first_detected_at"], "2026-11-15T07:30:00Z");
        // An unprovable draft is review_required, never retried.
        let patch = repair_note_attempt(&target, "composer_occupied", base, now);
        assert_eq!(patch["repair"]["status"], "review_required");
        assert!(patch["repair"]["next_retry_at_ms"].is_null());
        // The cap exhausts scheduled retries.
        let mut tired = target.clone();
        tired["repair"]["attempts"] = json!(REPAIR_MAX_ATTEMPTS - 1);
        let patch = repair_note_attempt(&tired, "invalid_request", base, now);
        assert_eq!(patch["repair"]["status"], "exhausted");
        assert!(patch["repair"]["next_retry_at_ms"].is_null());
        // Success clears the record.
        assert!(repair_cleared()["repair"].is_null());
    }
}
