use anyhow::{Context, Result};
use chromiumoxide::cdp::browser_protocol::target::{CloseTargetParams, GetTargetsParams};
use chromiumoxide::error::{CdpError, ChannelError};
use chromiumoxide::{Browser, Page};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::time::{Instant, timeout};

use super::{BrowserSurfaces, RetiredPage, Session};

pub(crate) const PAGE_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const RENDERER_TIMEOUT_LIMIT: u8 = 3;

pub(super) struct Viewer(Arc<AtomicUsize>);

impl Drop for Viewer {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Release);
    }
}

impl BrowserSurfaces {
    pub(super) async fn register_viewer(&self, key: &str) -> Option<Viewer> {
        let operation = self.key_operation(key).await;
        let _operation = operation.lock().await;
        let mut sessions = self.sessions.lock().await;
        let session = sessions.get_mut(key)?;
        session.idle_since = None;
        session.viewers.fetch_add(1, Ordering::AcqRel);
        Some(Viewer(Arc::clone(&session.viewers)))
    }

    pub(crate) async fn actor_probe_timed_out(
        &self,
        key: &str,
        expected_target: &str,
        can_replace: bool,
    ) -> Result<bool> {
        let operation = self.key_operation(key).await;
        let _operation = operation.lock().await;
        let session = {
            let mut sessions = self.sessions.lock().await;
            let Some(session) = sessions.get_mut(key).filter(|s| {
                s.shared_browser
                    && s.metadata["actor_identity"].is_string()
                    && s.page.target_id().as_ref() == expected_target
            }) else {
                return Ok(false);
            };
            session.idle_since = None;
            session.renderer_timeouts = session.renderer_timeouts.saturating_add(1);
            session.metadata["prompt_readiness"] = json!({
                "ready":false,"state":"inspection_failed","checked_at":cccc_contracts::utc_now(),
                "message":"The browser page did not respond to inspection."
            });
            if session.renderer_timeouts < RENDERER_TIMEOUT_LIMIT {
                return Ok(false);
            }
            if session.metadata["renderer_unresponsive"] != true {
                tracing::warn!(key, "Managed actor browser renderer is unresponsive");
            }
            session.metadata["renderer_unresponsive"] = json!(true);
            session.metadata["prompt_readiness"]["state"] = json!("renderer_unresponsive");
            session.metadata["prompt_readiness"]["message"] = json!(
                "The browser renderer is unresponsive. CCCC is checking whether its page can be replaced safely."
            );
            if !can_replace
                || session.last_observed_busy
                || session.viewers.load(Ordering::Acquire) > 0
            {
                return Ok(false);
            }
            session.renderer_timeouts = 0;
            session.clone()
        };
        self.retire_shared_page(key, &session, "renderer_unresponsive")
            .await
    }

    pub(crate) async fn consider_actor_idle_close(
        &self,
        key: &str,
        expected_target: &str,
        eligible: bool,
        idle_for: Duration,
    ) -> Result<bool> {
        let operation = self.key_operation(key).await;
        let _operation = operation.lock().await;
        let session = {
            let mut sessions = self.sessions.lock().await;
            let Some(session) = sessions.get_mut(key).filter(|s| {
                s.shared_browser
                    && s.metadata["actor_identity"].is_string()
                    && s.page.target_id().as_ref() == expected_target
            }) else {
                return Ok(false);
            };
            if !eligible || session.viewers.load(Ordering::Acquire) > 0 {
                session.idle_since = None;
                return Ok(false);
            }
            if session
                .idle_since
                .get_or_insert_with(Instant::now)
                .elapsed()
                < idle_for
            {
                return Ok(false);
            }
            session.idle_since = None;
            session.clone()
        };
        self.retire_shared_page(key, &session, "idle").await
    }

    // Caller holds the key lock. The profile lock serializes target creation
    // and retirement with other actors sharing the same browser.
    async fn retire_shared_page(
        &self,
        key: &str,
        session: &Session,
        reason: &'static str,
    ) -> Result<bool> {
        let Some((profile, operation)) = self.operation_for_key(key).await else {
            return Ok(false);
        };
        let _operation = operation.lock().await;
        let target_id = session.page.target_id();
        let closed = timeout(PAGE_PROBE_TIMEOUT, async {
            let owner = session.owner.read().await;
            if owner.handler.is_finished() {
                return Ok::<_, anyhow::Error>(false);
            }
            let browser = &owner.browser;
            let targets = browser.execute(GetTargetsParams::default()).await?;
            if targets
                .result
                .target_infos
                .iter()
                .any(|target| &target.target_id == target_id)
            {
                if reason == "idle"
                    && !targets
                        .result
                        .target_infos
                        .iter()
                        .any(|target| &target.target_id != target_id && target.r#type == "page")
                {
                    return Ok(false);
                }
                if reason == "idle" {
                    let ready = Self::inspect_prompt_readiness(&session.page).await?;
                    if ready["tab_url"].as_str() != Some(session.url.as_str())
                        || !super::prompt_submission::composer_is_idle(&ready)
                        || session
                            .page
                            .evaluate("document.hasFocus()")
                            .await?
                            .into_value::<bool>()?
                    {
                        return Ok(false);
                    }
                }
                let _ = browser
                    .execute(CloseTargetParams::new(target_id.clone()))
                    .await;
                loop {
                    let targets = browser.execute(GetTargetsParams::default()).await?;
                    if targets
                        .result
                        .target_infos
                        .iter()
                        .all(|target| &target.target_id != target_id)
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
            Ok(true)
        })
        .await
        .context("confirm actor browser target retirement")??;
        if closed {
            self.sessions.lock().await.remove(key);
            let mut key_profiles = self.key_profiles.lock().await;
            if key_profiles.get(key) == Some(&profile) {
                key_profiles.remove(key);
            }
            self.closed_pages.lock().await.insert(
                key.to_owned(),
                RetiredPage {
                    generation: session.metadata["actor_identity"]
                        .as_str()
                        .map(str::to_owned),
                    reason,
                },
            );
            tracing::info!(key, reason, "Retired managed actor browser page");
        }
        Ok(closed)
    }
}

pub(super) fn is_page_gone(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        let message = cause.to_string().to_ascii_lowercase();
        message.contains("receiver is gone")
            || message.contains("target closed")
            || message.contains("session closed")
            || message.contains("session with given id not found")
            || message.contains("no such target")
    })
}

/// A page handle can outlive its target during session restoration. Only skip
/// it after the browser itself responds and confirms that target has gone.
/// In particular, a canceled URL request can also mean the whole handler died.
pub(super) async fn candidate_page_url(browser: &Browser, page: &Page) -> Result<Option<String>> {
    match page.url().await {
        Ok(url) => Ok(Some(url.unwrap_or_default())),
        Err(error) => {
            confirm_candidate_gone(browser, page, error.into()).await?;
            Ok(None)
        }
    }
}

pub(super) async fn confirm_candidate_gone(
    browser: &Browser,
    page: &Page,
    error: anyhow::Error,
) -> Result<()> {
    let canceled = matches!(
        error.downcast_ref::<CdpError>(),
        Some(CdpError::ChannelSendError(ChannelError::Canceled(_)))
    );
    if !canceled && !is_page_gone(&error) {
        return Err(error);
    }
    let targets = browser
        .execute(GetTargetsParams::default())
        .await
        .context("check browser after candidate page disappeared")?;
    if targets
        .result
        .target_infos
        .iter()
        .any(|target| &target.target_id == page.target_id())
    {
        return Err(error);
    }
    Ok(())
}

pub(super) async fn recover_page(session: &mut Session) -> Result<()> {
    let owner = std::sync::Arc::clone(&session.owner);
    let owner = owner.read().await;
    let mut stale_pages = Vec::new();
    let mut live_page = None;
    for page in owner.browser.pages().await? {
        let Some(url) = candidate_page_url(&owner.browser, &page).await? else {
            continue;
        };
        if is_internal_page(&url) {
            stale_pages.push(page);
        } else {
            live_page = Some(page);
            break;
        }
    }
    let page = match live_page {
        Some(page) => page,
        None => owner
            .browser
            .new_page(&session.url)
            .await
            .context("recreate closed browser tab")?,
    };
    for stale_page in stale_pages {
        let _ = stale_page.close().await;
    }
    session.url = page.url().await?.unwrap_or_else(|| session.url.clone());
    session.page = page;
    session.updated_at = cccc_contracts::utc_now();
    Ok(())
}

pub(super) async fn close_internal_pages(browser: &Browser, active: &Page) -> Result<()> {
    let active_id = active.target_id();
    for page in browser.pages().await? {
        if page.target_id() == active_id {
            continue;
        }
        let Some(url) = candidate_page_url(browser, &page).await? else {
            continue;
        };
        if is_internal_page(&url) {
            let _ = page.close().await;
        }
    }
    Ok(())
}

pub(super) fn is_internal_page(url: &str) -> bool {
    url.is_empty()
        || url == "about:blank"
        || url.starts_with("chrome://new-tab-page")
        || url.starts_with("chrome://newtab")
}

#[cfg(test)]
mod tests {
    use super::is_page_gone;

    #[test]
    fn classifies_only_closed_cdp_page_errors_as_recoverable() {
        assert!(is_page_gone(&anyhow::anyhow!(
            "send failed because receiver is gone"
        )));
        assert!(is_page_gone(&anyhow::anyhow!("Target closed")));
        assert!(is_page_gone(&anyhow::anyhow!(
            "Session with given id not found"
        )));
        assert!(!is_page_gone(&anyhow::anyhow!(
            "Chromium loaded an internal network error page"
        )));
    }
}
