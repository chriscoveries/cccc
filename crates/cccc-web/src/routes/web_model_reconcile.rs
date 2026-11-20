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
    // A delivery reviewed after it went ambiguous belongs to the reviewer.
    if target["last_delivery_reviewed_at"]
        .as_str()
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .is_some_and(|reviewed| reviewed >= at)
    {
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
}
