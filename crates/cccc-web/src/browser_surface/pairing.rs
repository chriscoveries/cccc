//! A pairing receipt must return to the same owned Page that received the code.
//! No navigation, active-tab lookup, browser recovery, or content conversion,
//! except one: a conversation that reached ChatGPT's maximum length is moved
//! to a fresh chat before pairing starts (cwf#853).
use super::BrowserSurfaces;
use anyhow::{Context, Result, bail};
use chromiumoxide::Page;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

// CCCC's setup message. The route sends it; the guard below recognizes a copy
// that an interrupted attempt left unsent in the composer. Every variant shares
// this head and tail; only the connector sentence between them differs (cwf#863
// names this instance's ChatGPT plugin when one is configured).
const PROMPT_HEAD: &str = "Connect this conversation to the Actor selected in CCCC. ";
const PROMPT_TAIL: &str = " This records a pending link; reply with the returned receipt exactly as plain text so CCCC can verify this window and activate the link. Subsequent CCCC tool calls will use that Actor's configured permissions. This setup does not read workspace files or run tasks; wait for the next task message. If ChatGPT requires my approval, ask me. If the call is blocked or fails, report the error.";
const STOCK_CONNECTOR: &str =
    "Send this temporary CCCC pairing code to cccc_pair through the same CCCC connector: ";
const CLEAR_READBACK: Duration = Duration::from_secs(2);

pub(crate) fn pairing_prompt(code: &str) -> String {
    format!("{PROMPT_HEAD}{STOCK_CONNECTOR}{{\"code\":\"{code}\"}}.{PROMPT_TAIL}")
}

/// The whole setup message with exactly one code, as the composer shows it, and
/// nothing added. Anything else in the composer may be a human's draft.
fn is_stale_pairing_prompt(text: &str) -> bool {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .strip_prefix(PROMPT_HEAD)
        .and_then(|rest| rest.strip_suffix(PROMPT_TAIL))
        .is_some_and(|mid| stale_prompt_code(mid).is_some())
}

/// The one-time code inside a connector sentence that is exactly one of the
/// pairing prompts CCCC sends: the stock prompt, or the cwf#863 named-instance
/// prompt whose plugin name must match itself.
fn stale_prompt_code(mid: &str) -> Option<&str> {
    let (before, rest) = mid.split_once("{\"code\":\"")?;
    let (code, after) = rest.split_once("\"}")?;
    if !is_pairing_code(code) {
        return None;
    }
    if before == STOCK_CONNECTOR {
        return (after == ".").then_some(code);
    }
    let (name, tail) = before
        .strip_prefix("Use ONLY the ChatGPT plugin named \"")
        .and_then(|rest| {
            rest.split_once(
                "\" (this CCCC instance). Send this temporary CCCC pairing code to that plugin's cccc_pair tool: ",
            )
        })?;
    if !tail.is_empty() || !plugin_name_shape(name) {
        return None;
    }
    let expected = format!(
        ". Other CCCC plugins in this account belong to different CCCC instances and cannot accept this code: do not call them, and if ChatGPT asks to approve a CCCC plugin with any other name, deny it and use \"{name}\" instead."
    );
    (after == expected).then_some(code)
}

fn is_pairing_code(code: &str) -> bool {
    code.strip_prefix("wm_pair_").is_some_and(|hex| {
        hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}

/// The plugin-name shape routes::web_model_instance::sanitize accepts.
fn plugin_name_shape(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= 64
        && !name
            .chars()
            .any(|c| c.is_control() || matches!(c, '"' | '`' | '{' | '}' | '\\'))
}

pub(crate) struct PairingPage {
    page: Page,
    initial_url: String,
    conversation: Option<String>,
}

impl BrowserSurfaces {
    pub(crate) async fn pairing_page(&self, key: &str) -> Result<PairingPage> {
        let page = self
            .sessions
            .lock()
            .await
            .get(key)
            .map(|s| s.page.clone())
            .context("pairing_browser_required")?;
        // cwf#853: a conversation ChatGPT closed for length can never take the
        // handshake (or any later turn). Pairing is an explicit request to
        // bind this Actor, so move the tab to a fresh chat first and pair
        // there. This is the only navigation pairing does, and it happens
        // before anything is typed or sent.
        // A draft in the composer is never discarded by this navigation.
        let full_and_empty = self.prompt_readiness(key).await.ok().is_some_and(|r| {
            r["state"] == super::conversation_limit::CONVERSATION_LENGTH_EXCEEDED_STATE
                && r["composer_chars"].as_u64().unwrap_or(0) == 0
        });
        if full_and_empty {
            self.open_fresh_chat_for_pairing(key).await?;
        }
        let initial_url = page.url().await?.unwrap_or_default();
        let conversation = cccc_core::web_model_connectors::conversation_url(&initial_url).ok();
        if conversation.is_none() && !new_chat_url(&initial_url) {
            bail!("pairing_browser_required");
        }
        let target = PairingPage {
            page,
            initial_url,
            conversation,
        };
        let readiness = self.prompt_readiness(key).await?;
        // A page that is not ready, or still answering, is the reason to report:
        // nothing in its composer is touched then.
        // cwf#821: say why the page is not ready when it is ChatGPT's
        // rate-limit/load-error page. Nothing has been changed at this point.
        if readiness["state"] == super::load_error::RATE_LIMITED_STATE {
            bail!("pairing_browser_rate_limited");
        }
        if readiness["state"] == super::load_error::LOAD_FAILED_STATE {
            bail!("pairing_browser_load_failed");
        }
        // cwf#853: still full after the fresh-chat attempt (or a new chat
        // that is somehow full): say so instead of a generic not-ready.
        if readiness["state"] == super::conversation_limit::CONVERSATION_LENGTH_EXCEEDED_STATE {
            bail!(super::conversation_limit::PAIRING_CONVERSATION_FULL);
        }
        if readiness["ready"] != true || readiness["running"] == true {
            bail!("pairing_browser_not_ready");
        }
        if readiness["composer_chars"].as_u64().unwrap_or(0) > 0 {
            target.clear_stale_prompt(self, key).await?;
        }
        target.check_before_send(self, key).await?;
        Ok(target)
    }
}

impl BrowserSurfaces {
    async fn open_fresh_chat_for_pairing(&self, key: &str) -> Result<()> {
        let page = self.page(key).await.context("pairing_browser_required")?;
        let current = page.url().await?.unwrap_or_default();
        let Some(fresh) = fresh_chat_url(&current) else {
            bail!(super::conversation_limit::PAIRING_CONVERSATION_FULL);
        };
        tracing::info!(
            key,
            "ChatGPT conversation reached its maximum length; opening a fresh chat for pairing"
        );
        super::navigation::goto_dom_content_loaded(&page, &fresh)
            .await
            .context("pairing_browser_not_ready")?;
        // The composer mounts after DOMContentLoaded; wait a bounded time.
        let deadline = tokio::time::Instant::now() + FRESH_CHAT_READY_TIMEOUT;
        loop {
            let readiness = self.prompt_readiness(key).await?;
            if readiness["ready"] == true
                || tokio::time::Instant::now() >= deadline
                || readiness["state"] == "sign_in_required"
                || readiness["state"] == "verification_required"
            {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }
}

const FRESH_CHAT_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// The new-chat page on the same ChatGPT origin as a full conversation.
/// Never a project (`/g/...`) page: moving an Actor into a project changes
/// the bound URL shape.
pub(super) fn fresh_chat_url(current: &str) -> Option<String> {
    chatgpt_page_url(current)
        .map(|url| format!("https://{}/", url.host_str().unwrap_or("chatgpt.com")))
}

impl PairingPage {
    /// Remove an earlier attempt's unsent setup message, and only that: a human
    /// draft, the message with anything added, or a pending file keeps the
    /// composer occupied. The text is compared and deleted in one page task, so
    /// typing cannot land in between, then read back before pairing goes on.
    async fn clear_stale_prompt(&self, surfaces: &BrowserSurfaces, key: &str) -> Result<()> {
        self.check_before_send(surfaces, key).await?;
        let text = self
            .page
            .evaluate(format!("({READ_COMPOSER_SCRIPT})()"))
            .await?
            .into_value::<Option<String>>()?
            .unwrap_or_default();
        if !is_stale_pairing_prompt(&text) {
            bail!("pairing_composer_occupied");
        }
        let cleared = self
            .page
            .evaluate(format!(
                "({CLEAR_COMPOSER_SCRIPT})({}, {})",
                json!({ "expected": text }),
                super::prompt_submission::COMPOSER_ATTACHMENTS_SCRIPT
            ))
            .await?
            .into_value::<bool>()?;
        if !cleared {
            bail!("pairing_composer_occupied");
        }
        let deadline = Instant::now() + CLEAR_READBACK;
        loop {
            let readiness = surfaces.prompt_readiness(key).await?;
            if readiness["composer_chars"].as_u64() == Some(0) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!("pairing_composer_occupied");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub(crate) async fn check_before_send(
        &self,
        surfaces: &BrowserSurfaces,
        key: &str,
    ) -> Result<()> {
        let url = self.current_url(surfaces, key).await?;
        if url != self.initial_url {
            bail!("pairing_target_changed");
        }
        let attachments = super::prompt_submission::composer_has_attachments(&self.page).await?;
        if attachments {
            bail!("pairing_composer_occupied");
        }
        Ok(())
    }

    async fn current_url(&self, surfaces: &BrowserSurfaces, key: &str) -> Result<String> {
        let same_target = surfaces
            .sessions
            .lock()
            .await
            .get(key)
            .is_some_and(|s| s.page.target_id() == self.page.target_id());
        if !same_target {
            bail!("pairing_target_changed");
        }
        self.page.url().await?.context("pairing_interrupted")
    }

    pub(crate) async fn receipt_url(
        &self,
        surfaces: &BrowserSurfaces,
        key: &str,
        prompt: &str,
        receipt: Option<&str>,
    ) -> Result<Option<String>> {
        let url = self.current_url(surfaces, key).await?;
        let conversation = cccc_core::web_model_connectors::conversation_url(&url).ok();
        if self
            .conversation
            .as_ref()
            .is_some_and(|expected| conversation.as_ref() != Some(expected))
            || (conversation.is_none() && !pending_new_chat_url(&url))
        {
            bail!("pairing_target_changed");
        }
        // ChatGPT first assigns /c/WEB:<client-id>, then a server conversation
        // URL. A handshake started on the new-chat page waits through that
        // transition, even if the receipt arrives first. Never bind this URL.
        // An existing conversation still fails the identity check above.
        if conversation.is_none() {
            return Ok(None);
        }
        let Some(receipt) = receipt.filter(|s| !s.is_empty()) else {
            return Ok(None);
        };
        let result = self
            .page
            .evaluate(super::chatgpt_dom::with_messages(
                RECEIPT_SCRIPT,
                &json!({"prompt":prompt,"receipt":receipt}).to_string(),
            ))
            .await?
            .into_value::<Value>()?;
        // Read location and DOM in one evaluation. Also reject a navigation
        // during inspection instead of associating an old DOM with a new URL.
        if result["url"].as_str() != Some(url.as_str())
            || self.current_url(surfaces, key).await? != url
        {
            bail!("pairing_target_changed");
        }
        Ok(if result["verified"] == true {
            conversation
        } else {
            None
        })
    }
}

fn new_chat_url(raw: &str) -> bool {
    chatgpt_page_url(raw).is_some_and(|url| url.path() == "/")
}

fn pending_new_chat_url(raw: &str) -> bool {
    chatgpt_page_url(raw).is_some_and(|url| {
        url.path() == "/"
            || url.path().strip_prefix("/c/").is_some_and(|id| {
                !id.contains('/') && super::prompt_submission::provisional_conversation_id(id)
            })
    })
}

fn chatgpt_page_url(raw: &str) -> Option<reqwest::Url> {
    reqwest::Url::parse(raw).ok().filter(|url| {
        url.scheme() == "https"
            && matches!(url.host_str(), Some("chatgpt.com" | "chat.openai.com"))
            && url.username().is_empty()
            && url.password().is_none()
            && url.port().is_none()
    })
}

// The text of the composer that readiness marked, normalized as delivery checks do.
const READ_COMPOSER_SCRIPT: &str = r#"() => {
  const input = document.querySelector('[data-cccc-web-model-composer="cccc-web-model-composer"]');
  if (!input) return null;
  const text = ('value' in input) ? input.value : (input.innerText || input.textContent || '');
  return String(text || '').replace(/ /g, ' ').replace(/\s+/g, ' ').trim();
}"#;

// Delete the marked composer's whole content only while it is still exactly the
// approved text with no file. Focusing runs the page's own focus handlers, which
// may change the box, so every check comes after it, right before the deletion.
const CLEAR_COMPOSER_SCRIPT: &str = r#"({expected}, hasAttachments) => {
  const input = document.querySelector('[data-cccc-web-model-composer="cccc-web-model-composer"]');
  if (!input) return false;
  input.focus();
  const text = ('value' in input) ? input.value : (input.innerText || input.textContent || '');
  if (document.activeElement !== input || hasAttachments()
      || String(text || '').replace(/\u00a0/g, ' ').replace(/\s+/g, ' ').trim() !== expected) return false;
  if (input instanceof HTMLTextAreaElement || input instanceof HTMLInputElement) {
    input.setSelectionRange(0, String(input.value || '').length);
  } else {
    const range = document.createRange();
    range.selectNodeContents(input);
    const selection = getSelection();
    selection.removeAllRanges();
    selection.addRange(range);
  }
  return document.execCommand('delete');
}"#;

const RECEIPT_SCRIPT: &str = r#"({prompt,receipt}, messages) => {
  const normalize = s => String(s || '').replace(/\s+/g, ' ').trim();
  const index = messages.findLastIndex(e => e.role === 'user' && normalize(e.node.innerText) === normalize(prompt));
  let verified = false;
  if (index >= 0) for (const e of messages.slice(index + 1)) {
    if (e.role === 'user') break;
    if (e.node.getClientRects().length && (e.node.innerText || '').includes(receipt)) { verified = true; break; }
  }
  return {url:location.href,verified};
}"#;

#[cfg(test)]
mod tests {
    #[test]
    fn full_conversation_rotates_to_the_same_origin_new_chat_page() {
        assert_eq!(
            super::fresh_chat_url("https://chatgpt.com/c/6af6b34c-1234"),
            Some("https://chatgpt.com/".to_owned())
        );
        assert_eq!(
            super::fresh_chat_url("https://chatgpt.com/g/g-p-abc/c/6af6b34c-1234"),
            Some("https://chatgpt.com/".to_owned()),
            "never a project page"
        );
        assert_eq!(
            super::fresh_chat_url("https://chat.openai.com/c/x"),
            Some("https://chat.openai.com/".to_owned())
        );
        assert_eq!(super::fresh_chat_url("https://example.com/c/x"), None);
        assert_eq!(super::fresh_chat_url("http://chatgpt.com/c/x"), None);
        assert!(super::new_chat_url(
            &super::fresh_chat_url("https://chatgpt.com/c/x").unwrap_or_default()
        ));
    }

    use super::*;

    #[tokio::test]
    async fn current_chatgpt_pairing_receipt_keeps_exact_user_and_assistant_boundaries() {
        if crate::system_browser_path().is_none() {
            return;
        }
        let _chrome_guard = super::super::chrome_test_guard().await;
        let temp = tempfile::tempdir().expect("fixture");
        let (url, server) = super::super::browser_surface_tests::local_page("<main></main>").await;
        let surfaces = BrowserSurfaces::default();
        surfaces
            .open("pairing-dom", &temp.path().join("profile"), &url, 800, 600)
            .await
            .expect("open");
        let page = surfaces
            .sessions
            .lock()
            .await
            .get("pairing-dom")
            .expect("session")
            .page
            .clone();
        let user = |text: &str| {
            format!(
                "<div data-content-search-unit-key><div data-user-message-bubble>{text}</div></div>"
            )
        };
        let assistant = |text: &str| {
            format!(
                "<div data-content-search-unit-key><h4 data-conversation-role=assistant>Assistant</h4><div>{text}</div></div>"
            )
        };
        let prompt = "Pair fixture-code with fixture-receipt";
        let receipt = "fixture-receipt";
        for (label, html, verified) in [
            (
                "returned receipt",
                format!("{}{}", user(prompt), assistant(receipt)),
                true,
            ),
            (
                "user quote is not a reply",
                format!("{}{}", user(prompt), assistant("No receipt")),
                false,
            ),
            (
                "earlier reply",
                format!("{}{}", assistant(receipt), user(prompt)),
                false,
            ),
            (
                "later user boundary",
                format!(
                    "{}{}{}",
                    user(prompt),
                    user("Different task"),
                    assistant(receipt)
                ),
                false,
            ),
            (
                "wrong setup code",
                format!("{}{}", user("Pair an old code"), assistant(receipt)),
                false,
            ),
            (
                "mixed DOM",
                format!(
                    "<article data-message-author-role=user>{}</article><article data-message-author-role=assistant>{}</article>",
                    user(prompt),
                    assistant(receipt)
                ),
                true,
            ),
        ] {
            page.evaluate(format!(
                "document.querySelector('main').innerHTML={}",
                json!(html)
            ))
            .await
            .expect("fixture DOM");
            let result = page
                .evaluate(super::super::chatgpt_dom::with_messages(
                    RECEIPT_SCRIPT,
                    &json!({"prompt":prompt,"receipt":receipt}).to_string(),
                ))
                .await
                .expect("receipt check")
                .into_value::<Value>()
                .expect("result");
            assert_eq!(result["verified"], verified, "{label}");
        }
        surfaces.close("pairing-dom").await.expect("close");
        server.abort();
    }

    #[test]
    fn only_the_whole_setup_message_with_one_code_is_stale() {
        let code = format!("wm_pair_{}", "0123456789abcdef".repeat(4));
        let prompt = pairing_prompt(&code);
        assert!(is_stale_pairing_prompt(&prompt));
        // As a composer may show it: wrapped, with no-break spaces.
        assert!(is_stale_pairing_prompt(&format!(
            " {}\n",
            prompt.replacen(". ", ".\u{a0}\n", 2)
        )));
        for draft in [
            String::new(),
            "Keep my private draft".to_owned(),
            format!("{prompt} Also check the build."),
            format!("Note: {prompt}"),
            prompt.replace(&code, &format!("{code} {code}")),
            prompt.replace(&code, "wm_pair_"),
            prompt.replace(&code, &code[..code.len() - 1]),
            prompt.replace(&code, &format!("wm_pair_{}", "0123456789ABCDEF".repeat(4))),
            prompt.replace("Connect this conversation", "Connect that conversation"),
        ] {
            assert!(!is_stale_pairing_prompt(&draft), "{draft}");
        }
    }

    #[test]
    fn the_named_instance_prompt_is_stale_too() {
        let code = format!("wm_pair_{}", "0123456789abcdef".repeat(4));
        let named = format!(
            "Connect this conversation to the Actor selected in CCCC. Use ONLY the ChatGPT plugin named \"CCCC alpha\" (this CCCC instance). Send this temporary CCCC pairing code to that plugin's cccc_pair tool: {{\"code\":\"{code}\"}}. Other CCCC plugins in this account belong to different CCCC instances and cannot accept this code: do not call them, and if ChatGPT asks to approve a CCCC plugin with any other name, deny it and use \"CCCC alpha\" instead. This records a pending link; reply with the returned receipt exactly as plain text so CCCC can verify this window and activate the link. Subsequent CCCC tool calls will use that Actor's configured permissions. This setup does not read workspace files or run tasks; wait for the next task message. If ChatGPT requires my approval, ask me. If the call is blocked or fails, report the error."
        );
        assert!(is_stale_pairing_prompt(&named));
        // The plugin name must match itself; a different second name is not
        // one of our prompts.
        let mismatched = named.replace("use \"CCCC alpha\" instead", "use \"Other\" instead");
        assert!(!is_stale_pairing_prompt(&mismatched));
    }

    #[test]
    fn only_native_new_chat_routes_can_remain_pending() {
        for url in [
            "https://chatgpt.com/",
            "https://chatgpt.com/?model=fixture",
            "https://chatgpt.com/c/WEB:00000000-0000-4000-8000-000000000001",
            "https://chatgpt.com/c/WEB%3A00000000-0000-4000-8000-000000000001",
            "https://chatgpt.com/c/local-chatgpt:00000000-0000-4000-8000-000000000001",
            "https://chatgpt.com/c/local-chatgpt%3A00000000-0000-4000-8000-000000000001",
            "https://chatgpt.com/c/local-chatgpt%3a00000000-0000-4000-8000-000000000001",
            "https://chat.openai.com/c/WEB:pending",
        ] {
            assert!(pending_new_chat_url(url), "{url}");
            assert!(
                cccc_core::web_model_connectors::conversation_url(url).is_err(),
                "pending URL must not grant authority: {url}"
            );
        }
        for url in [
            "https://chatgpt.com/c/stable-id",
            "https://chatgpt.com/c/WEB:pending/other",
            "https://chatgpt.com/c/local-chatgpt%3Apending/other",
            "https://chatgpt.com/auth/login",
            "http://chatgpt.com/c/WEB:pending",
            "https://chatgpt.com.example/c/WEB:pending",
            "https://other.chatgpt.com/c/WEB:pending",
            "https://~@chatgpt.com/c/WEB:pending",
            "https://chatgpt.com:8443/c/WEB:pending",
            "about:blank",
        ] {
            assert!(!pending_new_chat_url(url), "{url}");
        }
    }
}
