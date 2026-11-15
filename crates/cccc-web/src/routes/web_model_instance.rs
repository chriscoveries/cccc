//! Instance identity for ChatGPT pairing (cwf#863).
//!
//! One ChatGPT account can have several CCCC plugins installed, one per CCCC
//! instance ("CCCC FIRESTAR", "CCCC macdad"). The pairing prompt used to say
//! only "the same CCCC connector", so ChatGPT sometimes called another
//! instance's `cccc_pair` and then sat on that plugin's approval prompt until
//! the pairing timed out. When the operator tells CCCC the display name of
//! its own plugin (`CCCC_WEB_MODEL_PLUGIN_NAME`), the prompt names it and
//! tells the model to refuse every other CCCC plugin.
use serde_json::{Map, Value};

const ENV_PLUGIN_NAME: &str = "CCCC_WEB_MODEL_PLUGIN_NAME";
const MAX_NAME_CHARS: usize = 64;

/// The configured display name of this instance's ChatGPT plugin, if any.
pub(super) fn plugin_name() -> Option<String> {
    sanitize(std::env::var(ENV_PLUGIN_NAME).ok().as_deref())
}

/// A display name safe to quote inside a prompt: trimmed, one line, no
/// quotes or braces, bounded length. Anything else is ignored.
pub(super) fn sanitize(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    if value.is_empty()
        || value.chars().count() > MAX_NAME_CHARS
        || value
            .chars()
            .any(|c| c.is_control() || matches!(c, '"' | '`' | '{' | '}' | '\\'))
    {
        return None;
    }
    Some(value.to_owned())
}

/// The pairing message typed into the ChatGPT conversation.
pub(super) fn pairing_prompt(code: &str, plugin: Option<&str>) -> String {
    let connector = match plugin {
        Some(name) => format!(
            "Use ONLY the ChatGPT plugin named \"{name}\" (this CCCC instance). Send this temporary CCCC pairing code to that plugin's cccc_pair tool: {{\"code\":\"{code}\"}}. Other CCCC plugins in this account belong to different CCCC instances and cannot accept this code: do not call them, and if ChatGPT asks to approve a CCCC plugin with any other name, deny it and use \"{name}\" instead."
        ),
        None => format!(
            "Send this temporary CCCC pairing code to cccc_pair through the same CCCC connector: {{\"code\":\"{code}\"}}. If several CCCC plugins are installed, use only the one this setup message came from; the code is valid on one CCCC instance only."
        ),
    };
    format!(
        "Connect this conversation to the Actor selected in CCCC. {connector} This records a pending link; reply with the returned receipt exactly as plain text so CCCC can verify this window and activate the link. Subsequent CCCC tool calls will use that Actor's configured permissions. This setup does not read workspace files or run tasks; wait for the next task message. If ChatGPT requires my approval, ask me. If the call is blocked or fails, report the error."
    )
}

/// A suffix for the cccc_pair tool description naming this instance.
pub(super) fn tool_description_suffix(plugin: Option<&str>) -> String {
    plugin
        .map(|name| format!(" This connector is the CCCC instance \"{name}\"; a code from another instance's setup message belongs to that instance's plugin."))
        .unwrap_or_default()
}

/// One automatic retry, and only for a handshake that simply ran out of
/// time (typically: the model called the wrong plugin, or an approval prompt
/// was never answered). Every other failure needs a person or a fixed tab.
pub(super) fn retry_after(reason: &str, retried: bool) -> bool {
    reason == "pairing_timeout" && !retried
}

/// Arguments for a fresh `web_model_pairing_begin` after a timed-out one.
pub(super) fn retry_begin_args(args: &Map<String, Value>) -> Map<String, Value> {
    let mut next = args.clone();
    next.remove("pairing_id");
    next.remove("error_code");
    next
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn names_this_instance_and_refuses_other_cccc_plugins() {
        let prompt = pairing_prompt("ABC123", Some("CCCC FIRESTAR"));
        assert!(prompt.contains("ONLY the ChatGPT plugin named \"CCCC FIRESTAR\""));
        assert!(prompt.contains("{\"code\":\"ABC123\"}"));
        assert!(prompt.contains("deny it"));
        assert!(prompt.contains("reply with the returned receipt exactly"));
        assert!(!prompt.contains("CCCC macdad"));
    }

    #[test]
    fn without_a_name_still_warns_about_multiple_plugins() {
        let prompt = pairing_prompt("XYZ", None);
        assert!(prompt.contains("{\"code\":\"XYZ\"}"));
        assert!(prompt.contains("valid on one CCCC instance only"));
        assert!(prompt.contains("reply with the returned receipt exactly"));
    }

    #[test]
    fn sanitizes_the_configured_name() {
        assert_eq!(sanitize(Some("  CCCC macdad ")), Some("CCCC macdad".into()));
        assert_eq!(sanitize(Some("")), None);
        assert_eq!(sanitize(None), None);
        assert_eq!(sanitize(Some("a\"b")), None);
        assert_eq!(sanitize(Some("line\nbreak")), None);
        assert_eq!(sanitize(Some(&"x".repeat(65))), None);
    }

    #[test]
    fn retries_a_timeout_exactly_once() {
        assert!(retry_after("pairing_timeout", false));
        assert!(!retry_after("pairing_timeout", true));
        assert!(!retry_after("pairing_composer_occupied", false));
        assert!(!retry_after("pairing_interrupted", false));
    }

    #[test]
    fn retry_begins_a_new_candidate() {
        let mut args = Map::new();
        args.insert("group_id".into(), json!("g"));
        args.insert("pairing_id".into(), json!("old"));
        args.insert("error_code".into(), json!("pairing_timeout"));
        let next = retry_begin_args(&args);
        assert_eq!(next.get("group_id"), Some(&json!("g")));
        assert!(next.get("pairing_id").is_none());
        assert!(next.get("error_code").is_none());
    }

    #[test]
    fn tool_description_names_the_instance_only_when_configured() {
        assert!(tool_description_suffix(Some("CCCC macdad")).contains("\"CCCC macdad\""));
        assert_eq!(tool_description_suffix(None), "");
    }
}
