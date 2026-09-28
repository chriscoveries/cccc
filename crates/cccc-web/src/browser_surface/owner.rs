use anyhow::{Context, Result};
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::network::CookieParam;
use chromiumoxide::handler::viewport::Viewport;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;

use super::{
    BrowserMode, profile_owner::ProfileLease, proxy::BrowserProxy, stop_browser,
    system_browser::SystemBrowserLaunch,
};

/// Process resources have one owner even when several surfaces share its pages.
pub(super) struct BrowserOwner {
    pub browser: Browser,
    pub handler: JoinHandle<()>,
    pub system_browser: Option<SystemBrowserLaunch>,
    profile_lease: Option<ProfileLease>,
    pub shared: bool,
    pub strategy: String,
    pub metadata: Value,
    pub viewer: Value,
    /// Monotonic origin for `activity`, so idleness never depends on wall clock.
    activity_base: Instant,
    /// Milliseconds since `activity_base` of the last CDP message from this process.
    activity: Arc<AtomicU64>,
}

impl BrowserOwner {
    pub async fn launch(
        profile: &Path,
        width: u32,
        height: u32,
        mode: BrowserMode,
        shared: bool,
        storage_state: Option<&Value>,
        actor_id: Option<&str>,
    ) -> Result<Self> {
        let mut profile_lease = ProfileLease::acquire(profile).await?;
        let mut system_browser = match mode {
            BrowserMode::Headless => None,
            BrowserMode::System { background } => {
                Some(SystemBrowserLaunch::prepare(width, height, background).await?)
            }
        };
        let proxy_args = BrowserProxy::from_env()?
            .map(|proxy| proxy.chromium_args())
            .unwrap_or_default();
        let launched = match &mut system_browser {
            Some(system_browser) => system_browser.launch(profile, proxy_args).await,
            None => {
                let mut config = BrowserConfig::builder()
                    .user_data_dir(profile)
                    .window_size(width, height)
                    .viewport(Viewport {
                        width,
                        height,
                        ..Viewport::default()
                    })
                    .new_headless_mode();
                if !proxy_args.is_empty() {
                    config = config.args(proxy_args);
                }
                // Attribute the browser process to its Actor: an operator (and the
                // process/disk reapers) must be able to tell whose window holds the
                // memory, and retire it without guessing. Kept after the proxy
                // block so a queue sibling that also edits this launch chain
                // applies independently (T178 queue clash with fix/web-bridge-misc).
                if let Some(actor_id) = actor_id {
                    config = config.env("CCCC_ACTOR_ID", actor_id);
                }
                // Preserve explicit CHROME and normal detection precedence.
                // Only try our additional binary names if that lookup fails.
                let config = config
                    .clone()
                    .build()
                    .or_else(|error| {
                        let (executable, _) =
                            super::system_browser::find_system_browser().ok_or(error)?;
                        config.chrome_executable(executable).build()
                    })
                    .map_err(anyhow::Error::msg)?;
                Browser::launch(config)
                    .await
                    .map(|(mut browser, handler)| {
                        let pid = browser
                            .get_mut_child()
                            .and_then(|child| child.as_mut_inner().id())
                            .unwrap_or_default();
                        (browser, handler, pid)
                    })
                    .map_err(anyhow::Error::from)
            }
        };
        let (mut browser, mut handler, browser_pid) = match launched {
            Ok(browser) => browser,
            Err(error) => {
                if let Some(system_browser) = &mut system_browser {
                    system_browser.stop().await;
                }
                return Err(error);
            }
        };
        let recorded = if system_browser.is_some() {
            profile_lease.record_pid(browser_pid).await
        } else {
            profile_lease.record_browser(&mut browser).await
        };
        if let Err(error) = recorded {
            let _ = browser.kill().await;
            if let Some(system_browser) = &mut system_browser {
                system_browser.stop().await;
            }
            return Err(error);
        }
        // CDP traffic is the honest idleness signal: a driven page keeps talking to
        // this handler, and a silent process is what an idle timeout is for. The
        // sweep reads `activity` without waking the browser.
        let activity_base = Instant::now();
        let activity = Arc::new(AtomicU64::new(0));
        let handler_activity = Arc::clone(&activity);
        let handler = tokio::spawn(async move {
            while let Some(message) = handler.next().await {
                if message.is_err() {
                    break;
                }
                handler_activity
                    .store(activity_base.elapsed().as_millis() as u64, Ordering::Relaxed);
            }
        });

        let (strategy, metadata) = system_browser.as_ref().map_or_else(
            || {
                (
                    "cdp_screencast".to_owned(),
                    json!({"visibility":"headless","display_owned":false,"pid":browser_pid}),
                )
            },
            |system| (system.strategy(), system.metadata(browser_pid, profile)),
        );
        let viewer = system_browser.as_ref().map_or_else(
            || json!({"kind":"screencast","vnc":{"available":false,"error":"unsupported_surface"}}),
            SystemBrowserLaunch::viewer,
        );
        let mut owner = Self {
            browser,
            handler,
            system_browser,
            profile_lease: Some(profile_lease),
            shared,
            strategy,
            metadata,
            viewer,
            activity_base,
            activity,
        };
        if let Some(cookies) = storage_state
            .and_then(|state| state.get("cookies"))
            .cloned()
        {
            let seeded = async {
                let cookies: Vec<CookieParam> =
                    serde_json::from_value(cookies).context("decode saved browser cookies")?;
                if !cookies.is_empty() {
                    owner.browser.set_cookies(cookies).await?;
                }
                Ok::<(), anyhow::Error>(())
            }
            .await;
            if let Err(error) = seeded {
                owner
                    .stop()
                    .await
                    .context("clean up browser after cookie initialization failed")?;
                return Err(error);
            }
        }
        Ok(owner)
    }

    /// Time since this process last spoke CDP, as of `now`. A long silence means
    /// nothing is driving its pages and the process only holds memory. The clock is
    /// a parameter so an idleness cutoff can be tested without waiting it out.
    pub(super) fn idle_for_at(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.activity_base)
            .saturating_sub(Duration::from_millis(self.activity.load(Ordering::Relaxed)))
    }

    pub async fn stop(&mut self) -> Result<()> {
        stop_browser(&mut self.browser, &mut self.handler).await?;
        if let Some(system) = &mut self.system_browser {
            system.stop().await;
        }
        if let Some(lease) = &mut self.profile_lease {
            lease.clear_owner()?;
        }
        // Other in-flight operations may retain an Arc to this stopped owner.
        // Release its lease only after process cleanup, not when the last Arc drops.
        self.profile_lease = None;
        Ok(())
    }
}
