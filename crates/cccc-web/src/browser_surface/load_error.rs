//! ChatGPT's "Could not load this ChatGPT conversation" page (cwf#821).
//!
//! When ChatGPT rate-limits the account (`429` from `/backend-api/conversation`),
//! any tab that reloads lands on an error page with a "Try again" button and no
//! composer, and stays there. Classify that page distinctly from a generic
//! `page_not_ready`, then recover with at most one reload per backoff window:
//! wait for `Retry-After` (bounded), reload once, re-check. Never a tight loop.
use std::time::Duration;

use anyhow::{Context, Result};
use chromiumoxide::Page;
use chromiumoxide::cdp::browser_protocol::network::EventResponseReceived;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::BrowserSurfaces;

pub(crate) const RATE_LIMITED_STATE: &str = "rate_limited";
pub(crate) const LOAD_FAILED_STATE: &str = "conversation_load_failed";
/// First wait when no `Retry-After` is known.
const RELOAD_BASE_DELAY: Duration = Duration::from_secs(30);
/// Backoff ceiling between reloads when `Retry-After` is unknown or small.
const RELOAD_MAX_BACKOFF: Duration = Duration::from_secs(5 * 60);
/// Upper bound on any single wait, even for a large `Retry-After`.
const RELOAD_MAX_DELAY: Duration = Duration::from_secs(15 * 60);
const MAX_RELOAD_ATTEMPTS: u32 = 3;
/// Margin added to `Retry-After` so the reload lands after the window closes.
const RETRY_AFTER_MARGIN: Duration = Duration::from_secs(2);
/// How long a recovery reload watches for the conversation response status.
const RELOAD_OBSERVE_WINDOW: Duration = Duration::from_secs(10);

/// DOM evidence read from a page that has no usable composer.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub(crate) struct LoadErrorProbe {
    /// Body text says "Could not load this (ChatGPT) conversation".
    pub load_failed_text: bool,
    /// Body text says "Too many requests".
    pub too_many_requests_text: bool,
    /// A visible "Try again" button.
    pub try_again_button: bool,
    /// Resource timing shows a 429 for `/backend-api/conversation...`.
    pub conversation_http_429: bool,
}

/// Only called for pages without a composer. A "Try again" button alone is
/// accepted there because the stranded page has no other controls.
pub(crate) fn classify_load_error(probe: &LoadErrorProbe) -> Option<&'static str> {
    if probe.conversation_http_429 || probe.too_many_requests_text {
        return Some(RATE_LIMITED_STATE);
    }
    if probe.load_failed_text || probe.try_again_button {
        return Some(LOAD_FAILED_STATE);
    }
    None
}

pub(crate) fn is_load_error_state(state: &str) -> bool {
    matches!(state, RATE_LIMITED_STATE | LOAD_FAILED_STATE)
}

/// Delay before the next recovery reload. `attempts` is the number of reloads
/// already made for this episode. Honour `Retry-After`, never below the
/// exponential backoff, and never above `RELOAD_MAX_DELAY`.
pub(crate) fn next_reload_delay(attempts: u32, retry_after_secs: Option<u64>) -> Duration {
    let backoff = RELOAD_BASE_DELAY
        .saturating_mul(1_u32 << attempts.min(16))
        .min(RELOAD_MAX_BACKOFF);
    let retry_after = retry_after_secs
        .map(|secs| Duration::from_secs(secs).saturating_add(RETRY_AFTER_MARGIN))
        .unwrap_or_default();
    backoff.max(retry_after).min(RELOAD_MAX_DELAY)
}

/// Parse a `Retry-After` value: delta-seconds only (HTTP-dates are ignored).
pub(crate) fn parse_retry_after(value: &str) -> Option<u64> {
    value.trim().parse::<u64>().ok()
}

fn is_conversation_api(url: &str, target: &str) -> bool {
    let Some(conversation) = super::prompt_submission::normalized_chatgpt_conversation_url(target)
        .and_then(|url| reqwest::Url::parse(&url).ok())
        .and_then(|url| super::prompt_submission::chatgpt_conversation_id(&url))
    else {
        return false;
    };
    reqwest::Url::parse(url).ok().is_some_and(|url| {
        matches!(url.host_str(), Some("chatgpt.com" | "chat.openai.com"))
            && url.path().strip_prefix("/backend-api/conversation/") == Some(conversation.as_str())
    })
}

pub(super) async fn probe_load_error(page: &Page) -> Result<LoadErrorProbe> {
    page.evaluate(format!("({LOAD_ERROR_SCRIPT})()"))
        .await
        .context("inspect ChatGPT load error page")?
        .into_value::<LoadErrorProbe>()
        .context("decode ChatGPT load error page")
}

pub(super) fn load_error_message(state: &str) -> &'static str {
    if state == RATE_LIMITED_STATE {
        "ChatGPT is rate limiting this account (HTTP 429), so the conversation could not load. Delivery is paused; CCCC reloads once after Retry-After, with backoff."
    } else {
        "ChatGPT shows \"Could not load this conversation\". Delivery is paused; CCCC reloads once after a backoff delay."
    }
}

#[derive(Default)]
struct ReloadObservation {
    status: Option<i64>,
    retry_after_secs: Option<u64>,
}

async fn observe_response(
    mut responses: impl futures_util::Stream<Item = std::sync::Arc<EventResponseReceived>> + Unpin,
    target: &str,
) -> ReloadObservation {
    tokio::time::timeout(RELOAD_OBSERVE_WINDOW, async {
        while let Some(event) = responses.next().await {
            if !is_conversation_api(&event.response.url, target) {
                continue;
            }
            return ReloadObservation {
                status: Some(event.response.status),
                retry_after_secs: (event.response.status == 429)
                    .then(|| retry_after_header(event.response.headers.inner()))
                    .flatten(),
            };
        }
        ReloadObservation::default()
    })
    .await
    .unwrap_or_default()
}

/// Reload once and watch the bound conversation's response for a bounded window.
async fn reload_and_observe(page: &Page, target: &str) -> Result<ReloadObservation> {
    if super::normalized_chatgpt_conversation_url(target).is_none() {
        page.reload().await.context("reload browser conversation")?;
        return Ok(ReloadObservation::default());
    }
    let responses = page
        .event_listener::<EventResponseReceived>()
        .await
        .context("listen for ChatGPT conversation responses")?;
    let current = page.url().await?.unwrap_or_default();
    if super::conversation_target_matches(target, &current) {
        page.reload().await.context("reload ChatGPT conversation")?;
    } else {
        super::navigation::goto_dom_content_loaded(page, target)
            .await
            .context("reopen saved ChatGPT conversation after a load error")?;
    }
    Ok(observe_response(responses, target).await)
}

fn retry_after_header(headers: &Value) -> Option<u64> {
    headers.as_object()?.iter().find_map(|(name, value)| {
        name.eq_ignore_ascii_case("retry-after")
            .then(|| value.as_str().and_then(parse_retry_after))
            .flatten()
    })
}

/// Stored with the bound actor's delivery target, so replacing a CDP page
/// cannot reset the retry budget or allow another request inside Retry-After.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct LoadRecovery {
    pub(crate) state: String,
    pub(crate) first_seen_at: String,
    pub(crate) reload_attempts: u32,
    pub(crate) retry_after_secs: Option<u64>,
    pub(crate) last_http_status: Option<i64>,
    pub(crate) next_reload_at_ms: Option<i64>,
    pub(crate) last_reload_failed: bool,
}

impl LoadRecovery {
    pub(crate) fn new(state: &str, now_ms: i64, retry_after_secs: Option<u64>) -> Self {
        let mut recovery = Self {
            state: state.to_owned(),
            first_seen_at: cccc_contracts::utc_now(),
            reload_attempts: 0,
            retry_after_secs,
            last_http_status: (state == RATE_LIMITED_STATE).then_some(429),
            next_reload_at_ms: None,
            last_reload_failed: false,
        };
        recovery.schedule(now_ms);
        recovery
    }

    /// Update the budget before touching Chrome, including for an I/O failure.
    pub(crate) fn book(&mut self, now_ms: i64) -> bool {
        if self
            .next_reload_at_ms
            .is_none_or(|deadline| deadline > now_ms)
            || self.reload_attempts >= MAX_RELOAD_ATTEMPTS
        {
            return false;
        }
        self.reload_attempts += 1;
        self.schedule(now_ms);
        true
    }

    pub(crate) fn apply_retry_after(&mut self, now_ms: i64, retry_after: u64) {
        self.retry_after_secs = Some(retry_after.max(self.retry_after_secs.unwrap_or_default()));
        self.last_http_status = Some(429);
        self.state = RATE_LIMITED_STATE.to_owned();
        self.schedule(now_ms);
    }

    fn schedule(&mut self, now_ms: i64) {
        let provider_wait = self.retry_after_secs.is_some_and(|secs| {
            Duration::from_secs(secs).saturating_add(RETRY_AFTER_MARGIN) > RELOAD_MAX_DELAY
        });
        self.next_reload_at_ms = (!provider_wait && self.reload_attempts < MAX_RELOAD_ATTEMPTS)
            .then(|| {
                now_ms
                    + next_reload_delay(self.reload_attempts, self.retry_after_secs).as_millis()
                        as i64
            });
    }
}

impl BrowserSurfaces {
    pub(super) async fn observe_initial_load(&self, key: &str, page: &Page, target: &str) {
        if super::normalized_chatgpt_conversation_url(target).is_none() {
            return;
        }
        let Some(hint) = self
            .sessions
            .lock()
            .await
            .get(key)
            .map(|session| std::sync::Arc::clone(&session.initial_retry_after))
        else {
            return;
        };
        let Ok(Ok(responses)) = tokio::time::timeout(
            Duration::from_secs(1),
            page.event_listener::<EventResponseReceived>(),
        )
        .await
        else {
            return;
        };
        let target = target.to_owned();
        tokio::spawn(async move {
            let observed = observe_response(responses, &target).await;
            *hint.lock().await = observed.retry_after_secs;
        });
    }

    pub(crate) async fn take_initial_retry_after(&self, key: &str) -> Option<u64> {
        let hint = self
            .sessions
            .lock()
            .await
            .get(key)?
            .initial_retry_after
            .clone();
        let value = hint.lock().await.take();
        value
    }

    pub(crate) async fn reload_conversation(
        &self,
        key: &str,
        target: &str,
        recovery: &mut LoadRecovery,
    ) {
        let observation =
            tokio::time::timeout(RELOAD_OBSERVE_WINDOW + Duration::from_secs(5), async {
                let page = self.page(key).await?;
                reload_and_observe(&page, target).await
            })
            .await;

        recovery.last_reload_failed = !matches!(&observation, Ok(Ok(_)));
        if let Ok(Ok(observed)) = observation {
            if observed.status.is_some() {
                recovery.last_http_status = observed.status;
            }
            if observed.status == Some(429) {
                recovery.state = RATE_LIMITED_STATE.to_owned();
            }
            if observed.retry_after_secs.is_some() {
                recovery.retry_after_secs = observed.retry_after_secs;
            }
        }
        recovery.schedule(chrono::Utc::now().timestamp_millis());
    }
}

pub(super) const LOAD_ERROR_SCRIPT: &str = r#"() => {
    const visible = node => {
        if (!node || node.closest('[aria-hidden="true"], [inert]')) return false;
        const rect = node.getBoundingClientRect();
        const style = getComputedStyle(node);
        return rect.width > 0 && rect.height > 0
            && style.display !== 'none' && style.visibility !== 'hidden';
    };
    const text = String((document.body && document.body.innerText) || '').replace(/\s+/g, ' ');
    const loadFailedText = /could not load this (chatgpt )?conversation/i.test(text);
    const tooManyRequestsText = /too many requests/i.test(text);
    const tryAgainButton = Array.from(document.querySelectorAll('button, [role="button"]'))
        .some(node => visible(node) && /^\s*try again\s*$/i.test(node.innerText || node.textContent || ''));
    let conversationHttp429 = false;
    try {
        conversationHttp429 = performance.getEntriesByType('resource').some(entry => {
            try {
                const path = new URL(entry.name).pathname;
                return (path === '/backend-api/conversation' || path.startsWith('/backend-api/conversation/'))
                    && entry.responseStatus === 429;
            } catch (_) { return false; }
        });
    } catch (_) {}
    return {
        load_failed_text: loadFailedText,
        too_many_requests_text: tooManyRequestsText,
        try_again_button: tryAgainButton,
        conversation_http_429: conversationHttp429
    };
}"#;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classifier_distinguishes_rate_limit_from_load_error_and_ordinary_unready_pages() {
        let none = LoadErrorProbe::default();
        assert_eq!(classify_load_error(&none), None, "plain page_not_ready");
        let stranded = LoadErrorProbe {
            load_failed_text: true,
            try_again_button: true,
            ..none
        };
        assert_eq!(classify_load_error(&stranded), Some(LOAD_FAILED_STATE));
        let try_again_only = LoadErrorProbe {
            try_again_button: true,
            ..none
        };
        assert_eq!(
            classify_load_error(&try_again_only),
            Some(LOAD_FAILED_STATE)
        );
        for probe in [
            LoadErrorProbe {
                conversation_http_429: true,
                ..stranded
            },
            LoadErrorProbe {
                too_many_requests_text: true,
                ..none
            },
            LoadErrorProbe {
                conversation_http_429: true,
                ..none
            },
        ] {
            assert_eq!(
                classify_load_error(&probe),
                Some(RATE_LIMITED_STATE),
                "{probe:?}"
            );
        }
        assert!(is_load_error_state(RATE_LIMITED_STATE));
        assert!(is_load_error_state(LOAD_FAILED_STATE));
        assert!(!is_load_error_state("page_not_ready"));
        assert!(!is_load_error_state("ready"));
    }

    #[test]
    fn reload_delay_honours_retry_after_with_capped_backoff() {
        assert_eq!(next_reload_delay(0, None), Duration::from_secs(30));
        assert_eq!(next_reload_delay(1, None), Duration::from_secs(60));
        assert_eq!(next_reload_delay(2, None), Duration::from_secs(120));
        assert_eq!(next_reload_delay(3, None), Duration::from_secs(240));
        assert_eq!(next_reload_delay(4, None), Duration::from_secs(300));
        assert_eq!(next_reload_delay(40, None), Duration::from_secs(300));
        // Retry-After of 1s never shortens the backoff: no tight loop.
        assert_eq!(next_reload_delay(0, Some(1)), Duration::from_secs(30));
        // A larger Retry-After wins, plus a small margin.
        assert_eq!(next_reload_delay(0, Some(85)), Duration::from_secs(87));
        assert_eq!(next_reload_delay(4, Some(600)), Duration::from_secs(602));
        // Bounded even for a huge Retry-After.
        assert_eq!(next_reload_delay(0, Some(86_400)), Duration::from_secs(900));
    }

    #[test]
    fn recovery_books_three_reloads_and_honors_long_provider_delays() {
        let mut recovery = LoadRecovery::new(RATE_LIMITED_STATE, 0, Some(85));
        assert!(!recovery.book(86_999));
        assert!(recovery.book(87_000));
        assert!(!recovery.book(87_000));
        assert!(recovery.book(174_000));
        assert!(!recovery.book(293_999));
        assert!(recovery.book(294_000));
        assert_eq!(recovery.reload_attempts, 3);
        assert_eq!(recovery.next_reload_at_ms, None);
        assert!(!recovery.book(i64::MAX));
        let paused = LoadRecovery::new(RATE_LIMITED_STATE, 0, Some(900));
        assert_eq!(paused.next_reload_at_ms, None);
    }

    #[test]
    fn retry_after_parsing_and_conversation_api_matching() {
        assert_eq!(parse_retry_after("85"), Some(85));
        assert_eq!(parse_retry_after(" 1 "), Some(1));
        assert_eq!(parse_retry_after("Wed, 21 Oct 2026 07:28:00 GMT"), None);
        assert_eq!(
            retry_after_header(&json!({"Content-Type":"x","Retry-After":"85"})),
            Some(85)
        );
        assert_eq!(retry_after_header(&json!({"retry-after":"7"})), Some(7));
        assert_eq!(retry_after_header(&json!({"x":"1"})), None);
        let target = "https://chatgpt.com/c/123";
        assert!(is_conversation_api(
            "https://chatgpt.com/backend-api/conversation/123",
            target,
        ));
        assert!(!is_conversation_api(
            "https://chatgpt.com/backend-api/conversation/456",
            target,
        ));
        assert!(!is_conversation_api(
            "https://chatgpt.com/backend-api/conversation",
            target,
        ));
        assert!(!is_conversation_api(
            "https://example.org/backend-api/conversation/123",
            target,
        ));
    }
}
