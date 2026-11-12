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
use serde::Deserialize;
use serde_json::{Value, json};

use super::BrowserSurfaces;

pub(crate) const RATE_LIMITED_STATE: &str = "rate_limited";
pub(crate) const LOAD_FAILED_STATE: &str = "conversation_load_failed";
/// First wait when no `Retry-After` is known.
const RELOAD_BASE_DELAY: Duration = Duration::from_secs(30);
/// Backoff ceiling between reloads when `Retry-After` is unknown or small.
const RELOAD_MAX_BACKOFF: Duration = Duration::from_secs(5 * 60);
/// Upper bound on any single wait, even for a large `Retry-After`.
const RELOAD_MAX_DELAY: Duration = Duration::from_secs(15 * 60);
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

fn is_conversation_api(url: &str) -> bool {
    reqwest::Url::parse(url).ok().is_some_and(|url| {
        matches!(url.host_str(), Some("chatgpt.com" | "chat.openai.com"))
            && (url.path() == "/backend-api/conversation"
                || url.path().starts_with("/backend-api/conversation/"))
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

struct ReloadObservation {
    status: Option<i64>,
    retry_after_secs: Option<u64>,
}

/// Reload once and watch the conversation API response for a bounded window.
async fn reload_and_observe(page: &Page) -> Result<ReloadObservation> {
    let mut responses = page
        .event_listener::<EventResponseReceived>()
        .await
        .context("listen for ChatGPT conversation responses")?;
    page.reload().await.context("reload ChatGPT conversation")?;
    let mut observed = ReloadObservation {
        status: None,
        retry_after_secs: None,
    };
    let _ = tokio::time::timeout(RELOAD_OBSERVE_WINDOW, async {
        while let Some(event) = responses.next().await {
            if !is_conversation_api(&event.response.url) {
                continue;
            }
            observed.status = Some(event.response.status);
            if event.response.status == 429 {
                observed.retry_after_secs = retry_after_header(event.response.headers.inner());
            }
            // The first conversation response decides whether the page loads.
            break;
        }
    })
    .await;
    Ok(observed)
}

fn retry_after_header(headers: &Value) -> Option<u64> {
    headers.as_object()?.iter().find_map(|(name, value)| {
        name.eq_ignore_ascii_case("retry-after")
            .then(|| value.as_str().and_then(parse_retry_after))
            .flatten()
    })
}

impl BrowserSurfaces {
    /// Self-heal a tab stranded on ChatGPT's load error page. Called with the
    /// readiness just inspected for `key`. Returns the recovery record that is
    /// also stored in the session metadata (`rate_limit`), or `None` when the
    /// page is not on the error page (which also clears an old record).
    pub(crate) async fn recover_load_error(
        &self,
        key: &str,
        readiness: &Value,
    ) -> Result<Option<Value>> {
        let state = readiness["state"].as_str().unwrap_or_default();
        if !is_load_error_state(state) {
            self.set_rate_limit_record(key, None).await;
            return Ok(None);
        }
        let now = chrono::Utc::now().timestamp_millis();
        let mut record = self
            .session_metadata(key, "rate_limit")
            .await
            .filter(Value::is_object)
            .unwrap_or_else(|| {
                let delay = next_reload_delay(0, None);
                json!({
                    "state":state,
                    "first_seen_at":cccc_contracts::utc_now(),
                    "reload_attempts":0,
                    "retry_after_secs":Value::Null,
                    "last_http_status":Value::Null,
                    "next_reload_at_ms":now + delay.as_millis() as i64,
                })
            });
        record["state"] = json!(state);
        if record["next_reload_at_ms"].as_i64().unwrap_or(0) > now {
            self.set_rate_limit_record(key, Some(record.clone())).await;
            return Ok(Some(record));
        }
        let page = self.page(key).await?;
        let observed = reload_and_observe(&page).await?;
        let attempts = record["reload_attempts"].as_u64().unwrap_or(0) as u32 + 1;
        let retry_after = observed
            .retry_after_secs
            .or_else(|| record["retry_after_secs"].as_u64());
        let delay = next_reload_delay(attempts, retry_after);
        record["reload_attempts"] = json!(attempts);
        record["last_reload_at"] = json!(cccc_contracts::utc_now());
        record["retry_after_secs"] = json!(retry_after);
        if observed.status.is_some() {
            record["last_http_status"] = json!(observed.status);
        }
        if observed.status == Some(429) {
            record["state"] = json!(RATE_LIMITED_STATE);
        }
        record["next_reload_at_ms"] =
            json!(chrono::Utc::now().timestamp_millis() + delay.as_millis() as i64);
        tracing::info!(
            key,
            attempts,
            status = observed.status,
            retry_after,
            next_delay_secs = delay.as_secs(),
            "Web Model tab reloaded once after ChatGPT load error"
        );
        self.set_rate_limit_record(key, Some(record.clone())).await;
        Ok(Some(record))
    }

    async fn session_metadata(&self, key: &str, field: &str) -> Option<Value> {
        self.sessions
            .lock()
            .await
            .get(key)
            .and_then(|session| session.metadata.get(field).cloned())
    }

    async fn set_rate_limit_record(&self, key: &str, record: Option<Value>) {
        let mut sessions = self.sessions.lock().await;
        let Some(metadata) = sessions
            .get_mut(key)
            .and_then(|session| session.metadata.as_object_mut())
        else {
            return;
        };
        match record {
            Some(record) => {
                metadata.insert("rate_limit".into(), record);
            }
            None => {
                metadata.remove("rate_limit");
            }
        }
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
        assert!(is_conversation_api(
            "https://chatgpt.com/backend-api/conversation/abc"
        ));
        assert!(is_conversation_api(
            "https://chatgpt.com/backend-api/conversation"
        ));
        assert!(!is_conversation_api(
            "https://chatgpt.com/backend-api/conversations?offset=0"
        ));
        assert!(!is_conversation_api(
            "https://example.com/backend-api/conversation/abc"
        ));
    }

    #[tokio::test]
    async fn load_error_script_reads_the_stranded_chatgpt_page() {
        if crate::system_browser_path().is_none() {
            return;
        }
        let _chrome_guard = super::super::chrome_test_guard().await;
        let temp = tempfile::tempdir().expect("fixture");
        let (url, server) = super::super::browser_surface_tests::local_page(
            "<main><div>Could not load this ChatGPT conversation</div><button>Try again</button></main>",
        )
        .await;
        let surfaces = BrowserSurfaces::default();
        surfaces
            .open("load-error", &temp.path().join("profile"), &url, 800, 600)
            .await
            .expect("open");
        let page = surfaces.page("load-error").await.expect("page");
        let probe = probe_load_error(&page).await.expect("probe");
        assert!(
            probe.load_failed_text && probe.try_again_button,
            "{probe:?}"
        );
        let readiness = surfaces
            .prompt_readiness("load-error")
            .await
            .expect("readiness");
        assert_eq!(readiness["ready"], false);
        assert_eq!(readiness["state"], LOAD_FAILED_STATE);
        surfaces.close("load-error").await.expect("close");
        server.abort();
    }
}
