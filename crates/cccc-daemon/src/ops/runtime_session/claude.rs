use super::{
    command_fingerprint, model_from_command, read, resume_enabled, string, valid_session_id,
    workspace_path, write,
};
use cccc_contracts::utc_now;
use cccc_core::HomeLayout;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::io;
use std::path::Path;

mod continuity;
mod retarget;
pub(crate) use retarget::{Retarget, validate_retarget, write_retarget};

const MANAGED_RECORD_VERSION: u64 = 2;
const MANAGED_TRANSPORT: &str = "claude_agent_view";

#[cfg(test)]
#[path = "claude_failure_tests.rs"]
mod failure_tests;

#[cfg(test)]
#[path = "claude_continuity_tests.rs"]
mod continuity_tests;

pub struct ResumeAttempt {
    pub session_id: String,
    pub attempt_id: String,
    pub model_change: Option<String>,
}

#[derive(Debug)]
pub struct ResumeBlocked(pub String);

impl std::fmt::Display for ResumeBlocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} Automatic resume is paused; retry with Start/Restart or choose New Session.",
            self.0
        )
    }
}

impl std::error::Error for ResumeBlocked {}

pub fn is_resume_blocked(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|inner| inner.is::<ResumeBlocked>())
}

pub fn prepare_managed(
    home: &HomeLayout,
    group_id: &str,
    actor_id: &str,
    cwd: &Path,
    base_command: &[String],
    environment: &BTreeMap<String, String>,
) -> std::io::Result<Option<ResumeAttempt>> {
    prepare_managed_with_policy(
        home,
        group_id,
        actor_id,
        cwd,
        base_command,
        environment,
        false,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn prepare_managed_with_policy(
    home: &HomeLayout,
    group_id: &str,
    actor_id: &str,
    cwd: &Path,
    base_command: &[String],
    environment: &BTreeMap<String, String>,
    explicit: bool,
) -> io::Result<Option<ResumeAttempt>> {
    let mut document = match read(home, group_id, actor_id) {
        Ok(document) => document,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::InvalidData
            ) =>
        {
            Map::new()
        }
        Err(error) => return Err(error),
    };
    let valid = document.get("v").and_then(Value::as_u64) == Some(MANAGED_RECORD_VERSION)
        && string(&document, "kind") == "runtime_session"
        && string(&document, "transport") == MANAGED_TRANSPORT
        && string(&document, "runtime") == "claude"
        && string(&document, "provider_thread_id").is_empty()
        && valid_session_id(&string(&document, "provider_session_id"));
    if !valid {
        continuity::decision(
            home,
            group_id,
            actor_id,
            &mut document,
            "skip",
            "no_valid_receipt",
        )?;
        return Ok(None);
    }
    if !resume_enabled() {
        return continuity::blocked(home, group_id, actor_id, &mut document, "resume_disabled");
    }
    if string(&document, "status") == "resume_failed" {
        let reason = string(&document, "last_resume_error");
        continuity::decision(
            home,
            group_id,
            actor_id,
            &mut document,
            "block",
            "previous_resume_failure",
        )?;
        return Err(io::Error::other(ResumeBlocked(reason)));
    }
    if string(&document, "status") != "usable"
        || document.get("resume_eligible").and_then(Value::as_bool) != Some(true)
    {
        return continuity::blocked(
            home,
            group_id,
            actor_id,
            &mut document,
            "resume_eligibility",
        );
    }
    let saved_cwd = std::path::PathBuf::from(string(&document, "workspace_path"));
    if !explicit && workspace_path(cwd) != workspace_path(&saved_cwd) {
        return continuity::blocked(home, group_id, actor_id, &mut document, "workspace_path");
    }
    let stripped = continuity::without_model(base_command);
    let legacy = continuity::prior_command(base_command, &string(&document, "model"));
    let exact_command =
        string(&document, "command_fingerprint") == command_fingerprint(base_command);
    let normalized_command =
        string(&document, "command_without_model_fingerprint") == command_fingerprint(&stripped);
    let legacy_command = string(&document, "command_fingerprint") == command_fingerprint(&legacy);
    if !(exact_command || normalized_command || legacy_command) {
        return continuity::blocked(
            home,
            group_id,
            actor_id,
            &mut document,
            "command_fingerprint",
        );
    }
    let identity = identity_fingerprint(&stripped, environment, &saved_cwd)?;
    let exact_identity = string(&document, "identity_fingerprint")
        == identity_fingerprint(base_command, environment, &saved_cwd)?;
    let normalized_identity = string(&document, "identity_without_model_fingerprint") == identity;
    let legacy_identity = legacy_command
        && string(&document, "identity_fingerprint")
            == identity_fingerprint(&legacy, environment, &saved_cwd)?;
    if !(exact_identity || normalized_identity || legacy_identity) {
        return continuity::blocked(
            home,
            group_id,
            actor_id,
            &mut document,
            "identity_fingerprint",
        );
    }
    let model = model_from_command(base_command);
    let model_change = (model != string(&document, "model")).then_some(model);
    let session_id = string(&document, "provider_session_id");
    let attempt_id = uuid::Uuid::new_v4().simple().to_string();
    document.insert("last_resume_attempt_at".into(), json!(utc_now()));
    document.insert("last_resume_attempt_id".into(), json!(attempt_id));
    let reason = if string(&document, "explicit_resume_session_id") == session_id {
        "explicit_retarget"
    } else if model_change.is_some() {
        "model_only_drift"
    } else {
        "saved_session"
    };
    continuity::decision(home, group_id, actor_id, &mut document, "attempt", reason)?;
    Ok(Some(ResumeAttempt {
        session_id,
        attempt_id,
        model_change,
    }))
}

pub(super) fn recovery_decision(
    home: &HomeLayout,
    group: &str,
    actor: &str,
    document: &mut Map<String, Value>,
    action: &str,
    reason: &str,
) -> io::Result<()> {
    continuity::decision(home, group, actor, document, action, reason)
}

/// Select the saved cwd only for explicit lifecycle recovery; account guards still apply.
pub fn explicit_workspace(
    home: &HomeLayout,
    group: &str,
    actor: &str,
) -> io::Result<Option<std::path::PathBuf>> {
    let mut document = match read(home, group, actor) {
        Ok(document) => document,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::InvalidData
            ) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    if document.get("v").and_then(Value::as_u64) != Some(MANAGED_RECORD_VERSION)
        || string(&document, "runtime") != "claude"
        || string(&document, "kind") != "runtime_session"
        || string(&document, "transport") != MANAGED_TRANSPORT
        || !valid_session_id(&string(&document, "provider_session_id"))
    {
        return Ok(None);
    }
    let path = std::path::PathBuf::from(string(&document, "workspace_path"));
    if !path.is_dir() {
        return match continuity::blocked(
            home,
            group,
            actor,
            &mut document,
            "workspace_path_unavailable",
        ) {
            Err(error) => Err(error),
            Ok(_) => unreachable!("blocked always returns an error"),
        };
    }
    Ok(Some(path))
}

pub fn archive_new_session(home: &HomeLayout, group: &str, actor: &str) -> io::Result<()> {
    let mut document = match read(home, group, actor) {
        Ok(document) => document,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::InvalidData
            ) =>
        {
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let history = continuity::archive(&document, "new_session");
    document.insert("previous_sessions".into(), json!(history));
    document.insert("provider_session_id".into(), json!(""));
    document.insert("status".into(), json!("new_session"));
    document.insert("resume_eligible".into(), json!(false));
    continuity::decision(
        home,
        group,
        actor,
        &mut document,
        "skip",
        "explicit_new_session",
    )
}

/// Preserve the original conversation and fence late failures from newer attempts/successes.
pub fn record_resume_failure(
    home: &HomeLayout,
    group_id: &str,
    actor_id: &str,
    attempt: &ResumeAttempt,
    diagnostic: &str,
    blocked: bool,
) -> io::Result<()> {
    let mut document = read(home, group_id, actor_id)?;
    if string(&document, "provider_session_id") != attempt.session_id
        || string(&document, "last_resume_attempt_id") != attempt.attempt_id
    {
        return Ok(());
    }
    document.insert("last_resume_error".into(), json!(diagnostic));
    let failures = document
        .get("failure_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    document.insert("failure_count".into(), json!(failures.saturating_add(1)));
    if blocked {
        document.insert("status".into(), json!("resume_failed"));
        document.insert("resume_eligible".into(), json!(false));
    }
    // A terminal attempt must not record the same failure twice.
    document.insert("last_resume_attempt_id".into(), json!(""));
    document.insert("updated_at".into(), json!(utc_now()));
    write(home, group_id, actor_id, &document)
}

/// Called only by an authorized explicit Actor lifecycle action, never auto-wake or polling.
pub fn retry_failed_resume(home: &HomeLayout, group_id: &str, actor_id: &str) -> io::Result<bool> {
    let mut document = match read(home, group_id, actor_id) {
        Ok(document) => document,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if string(&document, "transport") != MANAGED_TRANSPORT
        || string(&document, "runtime") != "claude"
        || string(&document, "status") != "resume_failed"
    {
        return Ok(false);
    }
    document.insert("status".into(), json!("usable"));
    document.insert("resume_eligible".into(), json!(true));
    document.insert("updated_at".into(), json!(utc_now()));
    write(home, group_id, actor_id, &document)?;
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
pub fn record_managed(
    home: &HomeLayout,
    group_id: &str,
    actor_id: &str,
    cwd: &Path,
    base_command: &[String],
    environment: &BTreeMap<String, String>,
    session_id: &str,
    resumed: bool,
) -> std::io::Result<()> {
    if !resume_enabled() {
        return Ok(());
    }
    if !valid_session_id(session_id) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid Claude Agent View session id",
        ));
    }
    let now = utc_now();
    let old = match read(home, group_id, actor_id) {
        Ok(document) => document,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::InvalidData
            ) =>
        {
            Map::new()
        }
        Err(error) => return Err(error),
    };
    let history = if valid_session_id(&string(&old, "provider_session_id"))
        && string(&old, "provider_session_id") != session_id
    {
        continuity::archive(&old, "provider_session_replaced")
    } else {
        continuity::previous_sessions(&old)
    };
    let mut document = Map::from_iter([
        ("v".into(), json!(MANAGED_RECORD_VERSION)),
        ("kind".into(), json!("runtime_session")),
        ("transport".into(), json!(MANAGED_TRANSPORT)),
        ("group_id".into(), json!(group_id)),
        ("actor_id".into(), json!(actor_id)),
        ("runtime".into(), json!("claude")),
        ("workspace_path".into(), json!(workspace_path(cwd))),
        (
            "command_fingerprint".into(),
            json!(command_fingerprint(base_command)),
        ),
        ("model".into(), json!(model_from_command(base_command))),
        (
            "identity_fingerprint".into(),
            json!(identity_fingerprint(base_command, environment, cwd)?),
        ),
        ("provider_session_id".into(), json!(session_id)),
        ("provider_thread_id".into(), json!("")),
        ("resume_command_hint".into(), json!("")),
        (
            "captured_from".into(),
            json!(if resumed {
                "claude_agent_view_resume"
            } else {
                "claude_agent_view_start"
            }),
        ),
        ("status".into(), json!("usable")),
        ("resume_eligible".into(), json!(true)),
        ("last_seen_at".into(), json!(now)),
        ("last_resume_attempt_at".into(), json!("")),
        ("last_resume_attempt_id".into(), json!("")),
        ("last_resume_error".into(), json!("")),
        ("failure_count".into(), json!(0)),
        ("updated_at".into(), json!(utc_now())),
    ]);
    let stripped = continuity::without_model(base_command);
    document.insert(
        "command_without_model_fingerprint".into(),
        json!(command_fingerprint(&stripped)),
    );
    document.insert(
        "identity_without_model_fingerprint".into(),
        json!(identity_fingerprint(&stripped, environment, cwd)?),
    );
    document.insert("previous_sessions".into(), json!(history));
    retarget::retain_model(&old, &mut document, session_id);
    continuity::decision(
        home,
        group_id,
        actor_id,
        &mut document,
        "provider_ready",
        if resumed {
            "saved_session"
        } else {
            "fresh_session"
        },
    )
}

fn identity_fingerprint(
    command: &[String],
    environment: &BTreeMap<String, String>,
    cwd: &Path,
) -> std::io::Result<String> {
    cccc_core::codex_voice_settings::ResolvedAgentRuntime {
        runtime: cccc_contracts::ActorRuntime::Claude,
        runtime_mode: cccc_contracts::RuntimeMode::default(),
        command: command.to_vec(),
        environment: environment.clone(),
    }
    .identity_fingerprint_at(cwd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cccc_core::GroupStore;

    #[test]
    fn managed_receipt_rejects_legacy_and_identity_changes() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        home.initialize().expect("initialize");
        let group = GroupStore::new(home.clone())
            .expect("store")
            .create("Claude receipt", "")
            .expect("group");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let command = vec!["claude".into(), "--model".into(), "sonnet".into()];
        let environment = BTreeMap::from([(
            "CLAUDE_CONFIG_DIR".into(),
            temp.path().join("claude-a").to_string_lossy().into_owned(),
        )]);
        let session_id = "52b41c61-e23c-4b7c-8b60-809c347451b5";

        record_managed(
            &home,
            &group.group_id,
            "claude-1",
            &workspace,
            &command,
            &environment,
            session_id,
            false,
        )
        .expect("record Claude session");
        let stored = read(&home, &group.group_id, "claude-1").expect("receipt");
        assert!(stored.get("runner").is_none());
        assert_eq!(
            prepare_managed(
                &home,
                &group.group_id,
                "claude-1",
                &workspace,
                &command,
                &environment,
            )
            .expect("prepare Claude resume")
            .as_ref()
            .map(|attempt| attempt.session_id.as_str()),
            Some(session_id)
        );

        let mut changed = environment;
        changed.insert(
            "CLAUDE_CONFIG_DIR".into(),
            temp.path().join("claude-b").to_string_lossy().into_owned(),
        );
        assert!(
            prepare_managed(
                &home,
                &group.group_id,
                "claude-1",
                &workspace,
                &command,
                &changed,
            )
            .is_err()
        );

        let mut changed_secret = BTreeMap::from([(
            "CLAUDE_CONFIG_DIR".into(),
            temp.path().join("claude-a").to_string_lossy().into_owned(),
        )]);
        changed_secret.insert("ANTHROPIC_API_KEY".into(), "replacement-key".into());
        assert!(
            prepare_managed(
                &home,
                &group.group_id,
                "claude-1",
                &workspace,
                &command,
                &changed_secret,
            )
            .is_err()
        );
    }

    #[test]
    fn managed_receipt_rejects_changed_file_backed_launch_input() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        home.initialize().expect("initialize");
        let group = GroupStore::new(home.clone())
            .expect("store")
            .create("Claude file identity", "")
            .expect("group");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        std::fs::write(workspace.join("prompt.txt"), "first prompt").expect("prompt");
        let command = vec![
            "claude".into(),
            "--system-prompt-file".into(),
            "prompt.txt".into(),
        ];
        let session_id = "52b41c61-e23c-4b7c-8b60-809c347451b5";
        record_managed(
            &home,
            &group.group_id,
            "claude-1",
            &workspace,
            &command,
            &BTreeMap::new(),
            session_id,
            false,
        )
        .expect("record session");
        assert_eq!(
            prepare_managed(
                &home,
                &group.group_id,
                "claude-1",
                &workspace,
                &command,
                &BTreeMap::new(),
            )
            .expect("same input")
            .as_ref()
            .map(|attempt| attempt.session_id.as_str()),
            Some(session_id)
        );

        std::fs::write(workspace.join("prompt.txt"), "changed prompt").expect("change prompt");
        assert!(
            prepare_managed(
                &home,
                &group.group_id,
                "claude-1",
                &workspace,
                &command,
                &BTreeMap::new(),
            )
            .is_err()
        );
    }
}
