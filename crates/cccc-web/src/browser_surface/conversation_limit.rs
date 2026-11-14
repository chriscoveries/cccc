//! ChatGPT's "maximum length for this conversation" banner (cwf#853).
//!
//! When a ChatGPT conversation grows past its length limit, ChatGPT shows
//!
//! > You've reached the maximum length for this conversation, but you can keep
//! > talking by starting a new chat.
//!
//! with a "Start new chat" button. The composer is still on the page and still
//! accepts text, and the user bubble may still render, but ChatGPT refuses the
//! turn. Without this check CCCC reports the tab as `ready` and records every
//! later batch as `submitted`, so work is silently lost.
//!
//! Detection is text-first: the banner sentence is the signal. A visible
//! "Start new chat" button is recorded as supporting evidence only, because a
//! normal ChatGPT page can show a "New chat" control in its sidebar.
use anyhow::{Context, Result};
use chromiumoxide::Page;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(crate) const CONVERSATION_LENGTH_EXCEEDED_STATE: &str = "conversation_length_exceeded";
pub(crate) const PAIRING_CONVERSATION_FULL: &str = "pairing_browser_conversation_full";

/// DOM evidence for the conversation length banner.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub(crate) struct ConversationLimitProbe {
    /// Visible page text matched the banner sentence.
    pub banner_text: bool,
    /// The matched sentence (bounded), for health and logs.
    pub banner_excerpt: String,
    /// A visible button labelled exactly "Start new chat".
    pub start_new_chat_button: bool,
}

impl ConversationLimitProbe {
    /// Build a probe from page text and button labels. The browser script
    /// produces the same fields; this is the pure form used by tests.
    pub(crate) fn from_text(text: &str, button_labels: &[&str]) -> Self {
        let excerpt = banner_excerpt(text);
        Self {
            banner_text: excerpt.is_some(),
            banner_excerpt: excerpt.unwrap_or_default(),
            start_new_chat_button: button_labels
                .iter()
                .any(|label| is_start_new_chat_label(label)),
        }
    }

    pub(crate) fn exceeded(&self) -> bool {
        self.banner_text
    }

    pub(crate) fn to_json(&self) -> Value {
        json!({
            "state":CONVERSATION_LENGTH_EXCEEDED_STATE,
            "banner_excerpt":self.banner_excerpt,
            "start_new_chat_button":self.start_new_chat_button,
            "checked_at":cccc_contracts::utc_now(),
        })
    }
}

fn normalize(text: &str) -> String {
    text.replace(['\u{2019}', '\u{2018}'], "'")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// The banner sentence, or `None`. Matches "reached the maximum length for
/// this conversation" (any apostrophe, case, or whitespace), which is the
/// stable core of ChatGPT's wording.
pub(crate) fn banner_excerpt(text: &str) -> Option<String> {
    const NEEDLE: &str = "reached the maximum length for this conversation";
    let normalized = normalize(text);
    let at = normalized.find(NEEDLE)?;
    // Back up to the start of the sentence ("you've ...") when it is close.
    let sentence_start = normalized[..at]
        .rfind(['.', '!', '?'])
        .map_or(0, |index| index + 1);
    let start = if at - sentence_start <= 40 {
        sentence_start
    } else {
        at
    };
    let excerpt: String = normalized[start..].trim().chars().take(160).collect();
    Some(excerpt)
}

pub(crate) fn is_start_new_chat_label(label: &str) -> bool {
    normalize(label) == "start new chat"
}

pub(super) fn conversation_limit_message() -> &'static str {
    "ChatGPT says this conversation has reached its maximum length. Delivery is paused and queued messages stay unread. Re-pair this Actor to a fresh chat (connect with replace:true, or choose new-chat delivery) to continue."
}

/// Raw DOM evidence. The browser only pre-filters; Rust decides.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawConversationLimit {
    /// Visible text blocks outside message bubbles that mention "maximum length".
    texts: Vec<String>,
    /// Visible button labels outside message bubbles that mention "new chat".
    buttons: Vec<String>,
}

pub(super) async fn probe_conversation_limit(page: &Page) -> Result<ConversationLimitProbe> {
    let raw = page
        .evaluate(format!("({CONVERSATION_LIMIT_SCRIPT})()"))
        .await
        .context("inspect ChatGPT conversation length banner")?
        .into_value::<RawConversationLimit>()
        .context("decode ChatGPT conversation length banner")?;
    let labels = raw.buttons.iter().map(String::as_str).collect::<Vec<_>>();
    Ok(ConversationLimitProbe::from_text(
        &raw.texts.join("\n"),
        &labels,
    ))
}

/// Reads visible text only, so a quoted banner inside hidden or inert nodes
/// does not count. Message bubbles are excluded: a user or assistant message
/// that merely quotes the sentence (for example in a bug report) must not
/// trip the check. Output is bounded.
pub(super) const CONVERSATION_LIMIT_SCRIPT: &str = r#"() => {
    const visible = node => {
        if (!node || node.closest('[aria-hidden="true"], [inert]')) return false;
        const rect = node.getBoundingClientRect();
        const style = getComputedStyle(node);
        return rect.width > 0 && rect.height > 0
            && style.display !== 'none' && style.visibility !== 'hidden';
    };
    const inMessage = node => !!(node && node.closest(
        '[data-message-author-role], [data-message-id], article[data-testid^="conversation-turn"]'));
    const norm = s => String(s || '').replace(/\s+/g, ' ').trim();
    const texts = [];
    const walker = document.createTreeWalker(document.body || document.documentElement, NodeFilter.SHOW_TEXT);
    for (let node = walker.nextNode(); node && texts.length < 4; node = walker.nextNode()) {
        if (!/maximum/i.test(node.nodeValue || '')) continue;
        const parent = node.parentElement;
        if (!parent || inMessage(parent) || !visible(parent)) continue;
        // The sentence may be split across inline nodes; climb a few levels
        // until the whole phrase is in one element's text.
        let block = parent;
        for (let depth = 0; depth < 4 && block.parentElement
            && !/maximum length for this conversation/i.test(norm(block.innerText || block.textContent)); depth++) {
            block = block.parentElement;
        }
        texts.push(norm(block.innerText || block.textContent || node.nodeValue).slice(0, 400));
    }
    const buttons = Array.from(document.querySelectorAll('button, [role="button"], a'))
        .filter(node => visible(node) && !inMessage(node))
        .map(node => norm(node.innerText || node.textContent || ''))
        .filter(label => /new chat/i.test(label) && label.length <= 40)
        .slice(0, 8);
    return { texts, buttons };
}"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// Test pages are served for the whole test; leaking one string is fine.
    fn leak(page: String) -> &'static str {
        Box::leak(page.into_boxed_str())
    }

    const BANNER: &str = "You've reached the maximum length for this conversation, but you can keep talking by starting a new chat.";

    #[test]
    fn detects_the_exact_chatgpt_banner_with_button() {
        let probe = ConversationLimitProbe::from_text(
            &format!("Earlier reply text.\n{BANNER}\nStart new chat"),
            &["Share", "Start new chat"],
        );
        assert!(probe.exceeded(), "{probe:?}");
        assert!(probe.start_new_chat_button);
        assert!(
            probe
                .banner_excerpt
                .starts_with("you've reached the maximum length for this conversation"),
            "{}",
            probe.banner_excerpt
        );
    }

    #[test]
    fn tolerates_curly_apostrophe_case_and_line_breaks() {
        for text in [
            "You\u{2019}ve reached the maximum length for this conversation, but you can keep talking by starting a new chat.",
            "YOU'VE REACHED THE MAXIMUM LENGTH FOR THIS CONVERSATION",
            "You've reached the maximum\n   length for\tthis conversation, but ...",
        ] {
            assert!(banner_excerpt(text).is_some(), "{text}");
        }
    }

    #[test]
    fn banner_text_alone_is_enough_without_the_button() {
        let probe = ConversationLimitProbe::from_text(BANNER, &[]);
        assert!(probe.exceeded());
        assert!(!probe.start_new_chat_button);
    }

    #[test]
    fn ordinary_pages_and_near_misses_are_not_flagged() {
        for text in [
            "",
            "Message ChatGPT",
            "You've reached your GPT-5.6 Pro limit. Responses will use another model until it resets.",
            "You've reached the maximum number of messages for now.",
            "This conversation is getting long. Consider starting a new chat.",
            "Could not load this ChatGPT conversation. Try again",
            "the maximum length for this field is 32 characters",
        ] {
            let probe = ConversationLimitProbe::from_text(text, &["New chat", "Send prompt"]);
            assert!(!probe.exceeded(), "{text}");
        }
    }

    #[test]
    fn start_new_chat_button_alone_never_flags_a_ready_page() {
        // A button without the sentence is supporting evidence only.
        let probe = ConversationLimitProbe::from_text("Message ChatGPT", &["Start new chat"]);
        assert!(probe.start_new_chat_button);
        assert!(!probe.exceeded());
        assert!(is_start_new_chat_label("  Start   new chat "));
        assert!(!is_start_new_chat_label("New chat"));
        assert!(!is_start_new_chat_label("Start new chat in project"));
    }

    #[test]
    fn excerpt_is_bounded_and_json_names_the_state() {
        let long = format!("{BANNER} {}", "x ".repeat(500));
        let probe = ConversationLimitProbe::from_text(&long, &["Start new chat"]);
        assert!(probe.banner_excerpt.chars().count() <= 160);
        let value = probe.to_json();
        assert_eq!(value["state"], CONVERSATION_LENGTH_EXCEEDED_STATE);
        assert_eq!(value["start_new_chat_button"], true);
    }

    #[tokio::test]
    async fn script_reads_the_banner_but_not_a_quoted_message() {
        if crate::system_browser_path().is_none() {
            return;
        }
        let _chrome_guard = super::super::chrome_test_guard().await;
        let temp = tempfile::tempdir().expect("fixture");
        let (url, server) = super::super::browser_surface_tests::local_page(leak(format!(
            "<main><div data-message-author-role=\"user\">quoting: {BANNER}</div>\
             <div role=\"alert\"><span>{BANNER}</span><button>Start new chat</button></div>\
             <div contenteditable=\"true\" id=\"prompt-textarea\"></div></main>"
        )))
        .await;
        let surfaces = super::super::BrowserSurfaces::default();
        surfaces
            .open("maxlen", &temp.path().join("profile"), &url, 800, 600)
            .await
            .expect("open");
        let page = surfaces.page("maxlen").await.expect("page");
        let probe = probe_conversation_limit(&page).await.expect("probe");
        assert!(
            probe.banner_text && probe.start_new_chat_button,
            "{probe:?}"
        );
        surfaces.close("maxlen").await.expect("close");
        server.abort();

        let (quoted_url, quoted_server) = super::super::browser_surface_tests::local_page(leak(
            format!("<main><div data-message-author-role=\"user\">{BANNER}</div></main>"),
        ))
        .await;
        surfaces
            .open(
                "maxlen-q",
                &temp.path().join("profile-q"),
                &quoted_url,
                800,
                600,
            )
            .await
            .expect("open");
        let page = surfaces.page("maxlen-q").await.expect("page");
        let probe = probe_conversation_limit(&page).await.expect("probe");
        assert!(!probe.banner_text, "{probe:?}");
        surfaces.close("maxlen-q").await.expect("close");
        quoted_server.abort();
    }
}
