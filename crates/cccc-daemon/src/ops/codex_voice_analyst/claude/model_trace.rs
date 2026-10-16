//! Optional bounded transition metadata; never terminal, account or model text.
use serde_json::{Map, Value, json};
use std::io::Write;

const PREFIX: &[u8] = b"[cccc-claude-model] ";
const MAX_RECORD_BYTES: usize = 4096;

pub(in crate::ops::codex_voice_analyst) fn emit(event: &'static str, metadata: Value) {
    let Some(record) = record(event, metadata) else {
        return;
    };
    tracing::info!(event, metadata = %record, "Claude model transition metadata");
    if std::env::var("CCCC_CLAUDE_MODEL_TRACE").as_deref() != Ok("1") {
        return;
    }
    let Ok(bytes) = serde_json::to_vec(&record) else {
        return;
    };
    if PREFIX.len() + bytes.len() >= MAX_RECORD_BYTES {
        return;
    }
    let mut stderr = std::io::stderr().lock();
    // Logging must never turn a successful lifecycle/fence into an error or
    // panic, including a closed inherited stderr pipe.
    let _ = stderr
        .write_all(PREFIX)
        .and_then(|_| stderr.write_all(&bytes))
        .and_then(|_| stderr.write_all(b"\n"));
}

fn record(event: &'static str, metadata: Value) -> Option<Value> {
    const EVENTS: &[&str] = &[
        "claude_model_transition_started",
        "claude_model_screen_evicted",
        "claude_model_screen_anchor_lost",
        "claude_model_screen_recovered",
        "claude_model_screen_unreadable",
        "claude_model_input_admitted",
        "claude_model_input_sent",
        "claude_model_pre_input_fresh_requested",
        "claude_model_fresh_cleanup",
        "claude_model_fresh_attempt_abandoned",
        "claude_model_fresh_dispatched",
        "claude_model_persisted_confirmed",
        "claude_model_post_input_failed",
    ];
    if !EVENTS.contains(&event) {
        return None;
    }
    let metadata = metadata.as_object()?;
    let mut record = Map::from_iter([
        ("event".into(), json!(event)),
        ("process_id".into(), json!(std::process::id())),
    ]);
    for (key, value) in metadata {
        let allowed = match key.as_str() {
            "transition_id" => value.as_str().is_some_and(|id| {
                id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit())
            }),
            "short" => value
                .as_str()
                .is_some_and(|id| id.len() == 8 && id.bytes().all(|byte| byte.is_ascii_hexdigit())),
            "stage" => matches!(
                value.as_str(),
                Some("pre_input" | "post_input" | "unspecified")
            ),
            "sequence_class" => {
                matches!(value.as_str(), Some("clear_then_home" | "home_then_clear"))
            }
            "reason" => value.as_str() == Some("unreadable_evicted_screen"),
            "screen_id" | "received_bytes" | "retained_bytes" | "retained_from"
            | "max_output_bytes" | "evicted_bytes" | "anchor_at" | "bytes_attempted"
            | "bytes_written" => value.as_u64().is_some(),
            "owned_launch"
            | "anchor_known"
            | "scrollback_clear"
            | "complete_frame"
            | "current_paint_complete"
            | "cleanup_confirmed"
            | "fresh_fallback_allowed"
            | "exact_attempt_retired"
            | "owned_cleanup_confirmed"
            | "configured_model_confirmed"
            | "model_input_sent" => value.is_boolean(),
            _ => false,
        };
        if allowed {
            record.insert(key.clone(), value.clone());
        }
    }
    Some(Value::Object(record))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn trace_metadata_allowlist_excludes_model_screen_account_and_auth_contents() {
        let value = record(
            "claude_model_screen_recovered",
            json!({
                "transition_id":"0123456789abcdef0123456789abcdef", "short":"abcdef12",
                "screen_id":1, "stage":"pre_input", "received_bytes":393216,
                "sequence_class":"home_then_clear", "scrollback_clear":true,
                "target_model":"never log model", "screen":"never log prompt",
                "auth":"never log auth", "account":"never log account",
                "stage_bad":"private", "reason":"private arbitrary reason"
            }),
        )
        .expect("typed metadata");
        let object = value.as_object().expect("record");
        assert_eq!(object["short"], "abcdef12");
        assert_eq!(object["stage"], "pre_input");
        for forbidden in [
            "target_model",
            "screen",
            "auth",
            "account",
            "stage_bad",
            "reason",
        ] {
            assert!(!object.contains_key(forbidden));
        }
        let bytes = serde_json::to_vec(&value).expect("JSON");
        assert!(PREFIX.len() + bytes.len() < MAX_RECORD_BYTES);
    }
}
