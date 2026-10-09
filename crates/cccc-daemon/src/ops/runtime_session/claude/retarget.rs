//! Explicit selection shares continuity guards and the bounded receipt archive.
use super::*;
use crate::dispatch::OpError;

pub(crate) struct Retarget {
    pub target: Map<String, Value>,
    pub rollback: Map<String, Value>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn validate_retarget(
    home: &HomeLayout,
    group_id: &str,
    actor_id: &str,
    cwd: &Path,
    command: &[String],
    environment: &BTreeMap<String, String>,
    session_id: &str,
) -> Result<Retarget, OpError> {
    if !resume_enabled() {
        return Err(OpError::new(
            "claude_resume_disabled",
            "Resume session requires CCCC_RUNTIME_RESUME to be enabled",
        ));
    }
    if !valid_session_id(session_id) {
        return Err(OpError::new(
            "invalid_session_id",
            "Claude session ID must be a UUID",
        ));
    }
    let original = read(home, group_id, actor_id).map_err(|error| {
        OpError::new(
            "claude_receipt_missing",
            format!(
                "A managed Claude receipt is required to verify this actor's identity: {error}"
            ),
        )
    })?;
    if string(&original, "kind") != "runtime_session"
        || string(&original, "transport") != MANAGED_TRANSPORT
        || string(&original, "runtime") != "claude"
        || original.get("v").and_then(Value::as_u64) != Some(MANAGED_RECORD_VERSION)
    {
        return Err(OpError::new(
            "claude_receipt_invalid",
            "Resume session requires a managed Claude Agent View receipt",
        ));
    }
    let stripped = continuity::without_model(command);
    let legacy = continuity::prior_command(command, &string(&original, "model"));
    let exact = string(&original, "command_fingerprint") == command_fingerprint(command)
        && string(&original, "identity_fingerprint")
            == identity_fingerprint(command, environment, cwd).map_err(OpError::io)?;
    let normalized = string(&original, "command_without_model_fingerprint")
        == command_fingerprint(&stripped)
        && string(&original, "identity_without_model_fingerprint")
            == identity_fingerprint(&stripped, environment, cwd).map_err(OpError::io)?;
    let compatible_legacy = string(&original, "command_fingerprint")
        == command_fingerprint(&legacy)
        && string(&original, "identity_fingerprint")
            == identity_fingerprint(&legacy, environment, cwd).map_err(OpError::io)?;
    if !(exact || normalized || compatible_legacy) {
        return Err(OpError::new(
            "claude_identity_mismatch",
            "The actor's account/config identity or launch configuration has changed. Restore the original CLAUDE_CONFIG_DIR and actor settings before resuming a saved session",
        ));
    }
    let retained_model = super::super::super::codex_voice_analyst::validate_claude_retarget(
        environment,
        cwd,
        session_id,
    )?;
    let mut target = original.clone();
    if string(&original, "provider_session_id") != session_id {
        target.insert(
            "previous_sessions".into(),
            json!(continuity::archive(&original, "resume_session")),
        );
    }
    target.insert("workspace_path".into(), json!(workspace_path(cwd)));
    target.insert(
        "command_fingerprint".into(),
        json!(command_fingerprint(command)),
    );
    target.insert(
        "identity_fingerprint".into(),
        json!(identity_fingerprint(command, environment, cwd).map_err(OpError::io)?),
    );
    target.insert(
        "command_without_model_fingerprint".into(),
        json!(command_fingerprint(&stripped)),
    );
    target.insert(
        "identity_without_model_fingerprint".into(),
        json!(identity_fingerprint(&stripped, environment, cwd).map_err(OpError::io)?),
    );
    target.insert(
        "model".into(),
        json!(retained_model.as_deref().unwrap_or("")),
    );
    target.insert("model_application".into(), json!({"action":"retained","retained_model":retained_model,"configured_model":model_from_command(command),"configured_model_applied":false}));
    target.insert("provider_session_id".into(), json!(session_id));
    target.insert("explicit_resume_session_id".into(), json!(session_id));
    target.insert("provider_thread_id".into(), json!(""));
    target.insert("status".into(), json!("usable"));
    target.insert("resume_eligible".into(), json!(true));
    target.insert("last_resume_error".into(), json!(""));
    target.insert("last_resume_attempt_id".into(), json!(""));
    target.insert("failure_count".into(), json!(0));
    target.insert("updated_at".into(), json!(utc_now()));
    let mut rollback = original.clone();
    if let Some(previous) = target.get("previous_sessions") {
        rollback.insert("previous_sessions".into(), previous.clone());
    }
    let history = continuity::archive(&target, "resume_session_launch_failed");
    rollback.insert("previous_sessions".into(), json!(history));
    Ok(Retarget { target, rollback })
}

pub(crate) fn write_retarget(
    home: &HomeLayout,
    group_id: &str,
    actor_id: &str,
    document: &Map<String, Value>,
) -> io::Result<()> {
    write(home, group_id, actor_id, document)
}

/// Recording an explicit selection must describe the retained model, not actor settings.
pub(super) fn retain_model(
    old: &Map<String, Value>,
    document: &mut Map<String, Value>,
    session_id: &str,
) {
    if string(old, "explicit_resume_session_id") == session_id {
        document.insert("model".into(), json!(string(old, "model")));
        if let Some(application) = old.get("model_application") {
            document.insert("model_application".into(), application.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn archive_is_bounded_idempotent_and_accepts_old_receipts() {
        let mut document = Map::new();
        for _ in 0..25 {
            document.insert(
                "provider_session_id".into(),
                json!(uuid::Uuid::new_v4().to_string()),
            );
            document.insert("workspace_path".into(), json!("/workspace"));
            document.insert("model".into(), json!("haiku"));
            document.insert(
                "previous_sessions".into(),
                json!(continuity::archive(&document, "resume_session")),
            );
            let once = document.clone();
            document.insert(
                "previous_sessions".into(),
                json!(continuity::archive(&document, "resume_session")),
            );
            assert_eq!(document, once);
        }
        let latest = document["previous_sessions"]
            .as_array()
            .expect("receipt fixture")
            .last()
            .expect("receipt fixture")
            .clone();
        document["previous_sessions"]
            .as_array_mut()
            .expect("receipt fixture")
            .push(latest);
        assert_eq!(continuity::archive(&document, "resume_session").len(), 20);
        assert_eq!(continuity::previous_sessions(&document).len(), 20);
    }
}
