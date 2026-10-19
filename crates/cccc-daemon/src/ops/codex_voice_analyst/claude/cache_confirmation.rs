//! Confirm only the current native cache-cost dialog, never a generic Enter retry.
use crate::ops::terminal_text;
use std::io;

const MAX_OUTPUT: usize = 128 * 1024;
#[derive(Default)]
pub(super) struct Screen {
    output: Vec<u8>,
}
impl Screen {
    pub(super) fn feed(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.output.len().saturating_add(bytes.len()) > MAX_OUTPUT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Claude model switch could not be confirmed: native screen exceeded limit",
            ));
        }
        self.output.extend_from_slice(bytes);
        Ok(())
    }
    fn current(&self) -> Option<String> {
        // Native synchronized repaint completion is required. A partial new
        // repaint must not authorize input against the previous screen.
        if !self.output.ends_with(b"\x1b[?2026l") {
            return None;
        }
        Some(terminal_text::render(
            &String::from_utf8_lossy(&self.output),
            false,
        ))
    }
    pub(super) fn empty_prompt(&self) -> bool {
        let Some(text) = self.current() else {
            return false;
        };
        let lines: Vec<_> = text.lines().map(str::trim).collect();
        let tail = &lines[lines.len().saturating_sub(5)..];
        tail.contains(&"❯")
            && tail
                .iter()
                .any(|line| line.starts_with("⏵⏵ bypass permissions on"))
            && !tail
                .iter()
                .any(|line| line.contains("Switch model?") || line.contains("No, go back"))
    }
    pub(super) fn cache_cost(&self, label: &str) -> bool {
        let Some(text) = self.current() else {
            return false;
        };
        let lines: Vec<_> = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();
        let Some(title) = lines.iter().rposition(|line| *line == "Switch model?") else {
            return false;
        };
        let modal = &lines[title..];
        // These are the whole native modal's remaining nonempty rows. Reject
        // hooks, drafts/cancel selection, another modal and text appended below.
        if modal.len() < 5
            || modal.len() > 7
            || modal[1] != "Your next response will be slower and use more tokens"
            || modal[modal.len() - 2] != format!("❯ 1. Yes, switch to {label}")
            || modal[modal.len() - 1] != "2. No, go back"
        {
            return false;
        }
        let explanation = modal[2..modal.len() - 2].join(" ");
        explanation
            == format!(
                "This conversation is cached for the current model. Switching to {label} means the full history gets re-read on your next message."
            )
    }
}

pub(super) fn label(model: &str, version: (u64, u64, u64)) -> Option<&'static str> {
    // Display aliases are native-version-specific. Unknown catalogs fail closed.
    if version != (2, 1, 293) {
        return None;
    }
    match model {
        "opus" | "claude-opus-5-5" => Some("Opus 5.5"),
        "sonnet" | "claude-sonnet-5-5" => Some("Sonnet 5.5"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const MODAL: &str = "Switch model?\r\nYour next response will be slower and use more tokens\r\nThis conversation is cached for the current model. Switching to Opus 5.5 means the full history gets re-read on\r\nyour next message.\r\n❯ 1. Yes, switch to Opus 5.5\r\n  2. No, go back";
    fn frame(text: &str) -> Vec<u8> {
        format!("\x1b[?2026h\x1b[2J\x1b[H{text}\x1b[?2026l").into_bytes()
    }
    #[test]
    fn exact_current_cache_dialog_only() {
        let mut screen = Screen::default();
        screen.feed(&frame(MODAL)).expect("fixture");
        assert!(screen.cache_cost("Opus 5.5"));
        assert!(!screen.cache_cost("Sonnet 5.5"));
        screen
            .feed(&frame("❯\r\n⏵⏵ bypass permissions on (shift+tab to cycle)"))
            .expect("fixture");
        assert!(!screen.cache_cost("Opus 5.5"));
        assert!(screen.empty_prompt());
    }
    #[test]
    fn hook_cancel_draft_ambiguous_and_delayed_flags_never_confirm() {
        for text in [
            MODAL.replace(
                "Your next response will be slower and use more tokens",
                "A PreModelSwitch hook asked you to confirm",
            ),
            MODAL
                .replace("❯ 1. Yes", "  1. Yes")
                .replace("  2. No", "❯ 2. No"),
            format!("{MODAL}\r\nOther confirmation?"),
            "Model changed; flags are still being saved\r\n❯".into(),
        ] {
            let mut screen = Screen::default();
            screen.feed(&frame(&text)).expect("fixture");
            assert!(!screen.cache_cost("Opus 5.5"));
        }
    }
    #[test]
    fn incomplete_redraw_and_overflow_fail_closed() {
        let mut screen = Screen::default();
        screen.feed(&frame(MODAL)).expect("fixture");
        screen.feed(b"\x1b[?2026h\x1b[2J").expect("fixture");
        assert!(!screen.cache_cost("Opus 5.5"));
        assert!(screen.feed(&vec![b'x'; MAX_OUTPUT]).is_err());
        assert_eq!(label("opus", (2, 1, 293)), Some("Opus 5.5"));
        assert_eq!(label("opus", (2, 1, 294)), None);
        assert_eq!(label("other", (2, 1, 293)), None);
    }
}
