//! RS-1: one fresh fallback after a failed managed resume.
//!
//! A managed launch may prepare a provider session id and try to resume it. When that attempt
//! fails, the id is the problem: retrying it produces the same failure, so the binding has to be
//! invalidated and the launch repeated once with resume disabled. Those rules are the same for every
//! provider, so they live here once instead of in each launcher:
//!
//! * the retry boundary is around the provider launch, after a resume id was prepared and before
//!   the managed session is attached;
//! * only the id that actually failed is invalidated, never a newer binding;
//! * exactly one fresh attempt is allowed, so a failing fresh launch is returned rather than
//!   replayed (`fresh_once`);
//! * the failed id travels in the lifecycle ledger event, because the receipt alone does not say
//!   which session was dropped.
//!
//! Turn completion is per protocol today, so this module owns only the retry rule. The admission
//! window and the exit predicate live in `actor_restart_backoff`, so RS-1 and RS-3 cannot drift
//! apart: both read admission as uptime, and a provider that exits before it is a fast failure
//! RS-3 counts.

use cccc_contracts::{Event, utc_now};
use cccc_core::{GroupStore, HomeLayout, ledger};
use serde_json::Value;
use serde_json::json;
use std::io;

/// What the launcher's loop should do next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Next {
    /// Launch now, resuming this id when one is present.
    Launch { resume_id: Option<String> },
    /// No further attempt is permitted; return the last error to the caller.
    Stop,
}

/// The one-fresh-fallback rule for a single managed launch.
#[derive(Debug)]
pub(crate) struct ResumeFallback {
    prepared: Option<String>,
    resume_id: Option<String>,
    fresh_used: bool,
}

impl ResumeFallback {
    pub(crate) fn new(resume_id: Option<String>) -> Self {
        Self {
            prepared: resume_id.clone(),
            resume_id,
            fresh_used: false,
        }
    }

    /// The resume argument for the attempt about to be made.
    pub(crate) fn next(&self) -> Next {
        match &self.resume_id {
            Some(resume_id) => Next::Launch {
                resume_id: Some(resume_id.clone()),
            },
            None if self.fresh_used => Next::Stop,
            None => Next::Launch { resume_id: None },
        }
    }

    /// Record a failed attempt, given the resume id that attempt used.
    ///
    /// The one fresh fallback exists to clear a doomed *resume*. A launch that prepared no id has
    /// nothing to invalidate, so its failure is returned rather than retried, and the fresh
    /// attempt that follows an invalidated resume may not be retried in turn.
    pub(crate) fn failed(&mut self, attempted: Option<&str>) -> ResumeFailure {
        match (self.prepared.as_deref(), attempted) {
            (Some(prepared), Some(attempted)) if prepared == attempted => {
                // The fresh attempt is now allowed, and only now: it is the one fallback.
                self.resume_id = None;
                ResumeFailure::Invalidate {
                    failed_id: attempted.to_owned(),
                }
            }
            // A failure naming an id this launch did not prepare may not poison anything, and the
            // launcher must not keep launching against state it no longer understands.
            (Some(_), Some(_)) => {
                self.resume_id = None;
                self.fresh_used = true;
                ResumeFailure::Stop
            }
            _ => {
                self.fresh_used = true;
                ResumeFailure::FreshExhausted
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ResumeFailure {
    /// Invalidate this id, then make exactly one fresh attempt.
    Invalidate { failed_id: String },
    /// The single fresh attempt is spent, or there was nothing to resume; return the error.
    FreshExhausted,
    /// The failure names an id this launch did not prepare; nothing may be invalidated or retried.
    Stop,
}

impl ResumeFailure {
    pub(crate) fn invalidate_id(&self) -> Option<&str> {
        match self {
            Self::Invalidate { failed_id } => Some(failed_id),
            _ => None,
        }
    }

    #[must_use]
    pub(crate) fn may_retry_fresh(&self) -> bool {
        matches!(self, Self::Invalidate { .. })
    }
}

/// Group, actor and runtime coordinates for the lifecycle event RS-1 requires.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FallbackContext<'a> {
    pub home: &'a HomeLayout,
    pub group_id: &'a str,
    pub actor_id: &'a str,
    pub runtime: &'static str,
}

impl FallbackContext<'_> {
    /// The receipt invalidation is provider specific; the ledger event is not.
    pub(crate) fn record_resume_failure(
        &self,
        failed_id: &str,
        error: &str,
        fresh_error: Option<&str>,
    ) {
        let mut event = Event::new("actor.resume_failed", self.group_id);
        event.by = "system".into();
        event.data = resume_failed_data(self.actor_id, self.runtime, failed_id, error, fresh_error)
            .as_object()
            .cloned()
            .unwrap_or_default();
        let store = match GroupStore::new(self.home.clone()) {
            Ok(store) => store,
            Err(store_error) => {
                tracing::warn!(
                    group_id = %self.group_id,
                    actor_id = %self.actor_id,
                    %store_error,
                    "failed to resolve the group store for the resume failure event"
                );
                return;
            }
        };
        let ledger_path = match store.ledger_path(self.group_id) {
            Ok(path) => path,
            Err(path_error) => {
                tracing::warn!(
                    group_id = %self.group_id,
                    actor_id = %self.actor_id,
                    %path_error,
                    "failed to resolve the ledger path for the resume failure event"
                );
                return;
            }
        };
        if let Err(append_error) = ledger::append(&ledger_path, &event) {
            tracing::warn!(
                group_id = %self.group_id,
                actor_id = %self.actor_id,
                %append_error,
                "failed to record the invalidated managed session in the ledger"
            );
        }
    }
}

#[must_use]
pub(crate) fn resume_failed_data(
    actor_id: &str,
    runtime: &str,
    failed_id: &str,
    error: &str,
    fresh_error: Option<&str>,
) -> Value {
    json!({
        "actor_id": actor_id,
        "runtime": runtime,
        "failed_provider_session_id": failed_id,
        "error": error,
        "fresh_attempt_error": fresh_error,
        "recovered": fresh_error.is_none(),
        "at": utc_now(),
    })
}

/// Writes the provider receipt's failure fields. Returns the error so a launcher can log it without
/// losing the original failure that triggered the retry.
pub(crate) fn invalidate_receipt(
    invalidate: impl FnOnce(&str, &str) -> io::Result<()>,
    failed_id: &str,
    error: &str,
) {
    if let Err(record_error) = invalidate(failed_id, error) {
        tracing::warn!(
            %record_error, %failed_id, %error,
            "failed to invalidate the managed session receipt; the fresh attempt still runs"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drives the loop shape the launchers use, so the tests exercise the rule and not a mock of it.
    fn run_launch(
        mut fallback: ResumeFallback,
        mut attempt: impl FnMut(Option<&str>) -> Result<&'static str, String>,
        mut invalidate: impl FnMut(&str, &str),
    ) -> Result<&'static str, String> {
        loop {
            let resume_id = match fallback.next() {
                Next::Launch { resume_id } => resume_id,
                Next::Stop => return Err("no further attempt permitted".to_owned()),
            };
            match attempt(resume_id.as_deref()) {
                Ok(session) => return Ok(session),
                Err(error) => {
                    let failure = fallback.failed(resume_id.as_deref());
                    if let Some(failed_id) = failure.invalidate_id() {
                        invalidate(failed_id, error.as_str());
                    }
                    if failure.may_retry_fresh() {
                        continue;
                    }
                    return Err(error);
                }
            }
        }
    }

    #[test]
    fn a_failed_resume_is_invalidated_and_retried_fresh_exactly_once() {
        let mut attempts: Vec<Option<String>> = Vec::new();
        let mut invalidated: Vec<(String, String)> = Vec::new();
        let mut outcomes = vec![
            Err("copied a requested session".to_owned()),
            Ok("fresh-session"),
        ]
        .into_iter();

        let session = run_launch(
            ResumeFallback::new(Some("session-abc".to_owned())),
            |resume| {
                attempts.push(resume.map(str::to_owned));
                outcomes.next().unwrap_or(Ok("fresh-session"))
            },
            |failed_id, error| invalidated.push((failed_id.to_owned(), error.to_owned())),
        )
        .expect("the fresh attempt recovers the launch");

        assert_eq!(session, "fresh-session");
        assert_eq!(
            attempts,
            vec![Some("session-abc".to_owned()), None],
            "the second attempt must not receive a resume argument"
        );
        assert_eq!(
            invalidated,
            vec![(
                "session-abc".to_owned(),
                "copied a requested session".to_owned()
            )],
            "exactly the id that failed is invalidated, with its error"
        );
    }

    #[test]
    fn a_failing_fresh_launch_is_returned_and_never_replayed() {
        let mut attempts = 0usize;
        let mut invalidated: Vec<(String, String)> = Vec::new();
        let mut outcomes = vec![
            Err("resume refused".to_owned()),
            Err("fresh launch refused".to_owned()),
        ]
        .into_iter();

        let error = run_launch(
            ResumeFallback::new(Some("session-abc".to_owned())),
            |_resume| {
                attempts += 1;
                outcomes
                    .next()
                    .unwrap_or(Err("unexpected third attempt".to_owned()))
            },
            |failed_id, error| invalidated.push((failed_id.to_owned(), error.to_owned())),
        )
        .expect_err("a failed fresh launch is returned to the caller");

        assert_eq!(error, "fresh launch refused");
        assert_eq!(attempts, 2, "no third attempt is made");
        assert_eq!(invalidated.len(), 1);
    }

    #[test]
    fn a_launch_without_a_prepared_id_is_never_retried() {
        // The fallback exists to clear a doomed resume; with nothing prepared there is no id to
        // invalidate, so the failure is returned instead of retried into the same dead provider.
        let mut attempts: Vec<Option<String>> = Vec::new();
        let mut outcomes = vec![Err("provider refused".to_owned())].into_iter();
        let error = run_launch(
            ResumeFallback::new(None),
            |resume| {
                attempts.push(resume.map(str::to_owned));
                outcomes
                    .next()
                    .unwrap_or(Err("unexpected retry".to_owned()))
            },
            |_, _| panic!("nothing was resumed, so nothing may be invalidated"),
        )
        .expect_err("a fresh-only launch failure is returned");
        assert_eq!(error, "provider refused");
        assert_eq!(attempts, vec![None]);
    }

    #[test]
    fn a_successful_resume_is_not_retried_or_invalidated() {
        let mut attempts = 0usize;
        let mut invalidated: Vec<(String, String)> = Vec::new();
        let session = run_launch(
            ResumeFallback::new(Some("session-ok".to_owned())),
            |_resume| {
                attempts += 1;
                Ok("resumed")
            },
            |failed_id, error| invalidated.push((failed_id.to_owned(), error.to_owned())),
        )
        .expect("resume succeeds");
        assert_eq!(session, "resumed");
        assert_eq!(attempts, 1);
        assert!(invalidated.is_empty());
    }

    #[test]
    fn only_the_prepared_id_may_be_invalidated() {
        let mut fallback = ResumeFallback::new(Some("session-abc".to_owned()));
        let failure = fallback.failed(Some("some-other-session"));
        assert!(!failure.may_retry_fresh(), "a foreign id stops the loop");
        assert!(
            failure.invalidate_id().is_none(),
            "a foreign id is not poisoned"
        );
        assert_eq!(fallback.next(), Next::Stop);
    }

    #[test]
    fn the_ledger_payload_carries_the_failed_id() {
        let payload = resume_failed_data("peer1", "claude", "session-abc", "resume refused", None);
        assert_eq!(payload["failed_provider_session_id"], json!("session-abc"));
        assert_eq!(payload["runtime"], json!("claude"));
        assert_eq!(payload["recovered"], json!(true));
        assert!(payload["at"].as_str().is_some_and(|at| !at.is_empty()));

        let unrecovered = resume_failed_data("peer1", "codex", "thread-9", "boom", Some("nope"));
        assert_eq!(unrecovered["recovered"], json!(false));
        assert_eq!(unrecovered["fresh_attempt_error"], json!("nope"));
    }

    #[test]
    fn receipt_invalidation_errors_do_not_mask_the_original_failure() {
        let mut attempted = false;
        let original = "original failure".to_owned();
        invalidate_receipt(
            |_, _| {
                attempted = true;
                Err(io::Error::other("receipt write refused"))
            },
            "session-abc",
            &original,
        );
        assert!(attempted, "the receipt write was attempted");
        assert_eq!(
            original, "original failure",
            "the original error is preserved"
        );
    }
}
