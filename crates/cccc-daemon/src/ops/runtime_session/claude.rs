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
mod identity_conflict;
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

#[cfg(test)]
#[path = "claude_identity_receipt_tests.rs"]
mod identity_receipt_tests;

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

// Optional resume may retain upstream fresh-start behavior when the lock
// itself cannot be acquired. Never downgrade a callback/write error, genuine
// contention, selected intent, an in-flight attempt, or prior failed evidence.
fn with_optional_resume_lock<T>(
    home: &HomeLayout,
    group: &str,
    actor: &str,
    fresh: T,
    operation: impl FnOnce() -> io::Result<T>,
) -> io::Result<T> {
    let result = super::with_receipt_lock(home, group, actor, operation);
    let Err(error) = &result else {
        return result;
    };
    if !super::is_receipt_lock_unavailable(error) || error.kind() == io::ErrorKind::WouldBlock {
        return result;
    }
    let ordinary = match read(home, group, actor) {
        Ok(document) => {
            let marker = |key: &str| {
                document
                    .get(key)
                    .is_some_and(|value| !value.is_null() && value.as_str() != Some(""))
            };
            !marker("explicit_resume_session_id")
                && !document
                    .get("explicit_resume_pending")
                    .is_some_and(|value| !value.is_null() && value.as_bool() != Some(false))
                && string(&document, "status") != "resume_failed"
                && !marker("last_resume_attempt_id")
                && !marker("last_resume_error")
                && !marker("last_failed_resume_model")
                && document
                    .get("failure_count")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    == 0
        }
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::InvalidData
            ) =>
        {
            true
        }
        Err(_) => false,
    };
    if ordinary {
        tracing::warn!(error_kind = ?error.kind(),
            "optional Claude resume skipped because receipt lock admission failed; using ordinary fresh-start policy");
        // The daemon has no global tracing subscriber. Keep this degraded-start
        // warning observable without logging receipt/error/account contents.
        let mut stderr = std::io::stderr().lock();
        let _ = std::io::Write::write_all(&mut stderr,
            b"[cccc] Optional Claude resume lock admission failed; using ordinary fresh-start policy.\n");
        Ok(fresh)
    } else {
        result
    }
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
    _explicit: bool,
) -> io::Result<Option<ResumeAttempt>> {
    // An ordinary launch with resume disabled is a fresh launch, including
    // when an older saved receipt exists. Do not rewrite it as a failure.
    if !resume_enabled() {
        // Disabling normal resume does not authorize silently retiring a known
        // foreign-conversation conflict. Ordinary receipts still start fresh.
        if let Ok(document) = read(home, group_id, actor_id) {
            identity_conflict::require_clear(&document)?;
        }
        return Ok(None);
    }
    with_optional_resume_lock(home, group_id, actor_id, None, || {
        let mut document = match read(home, group_id, actor_id) {
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
        let valid = document.get("v").and_then(Value::as_u64) == Some(MANAGED_RECORD_VERSION)
            && string(&document, "kind") == "runtime_session"
            && string(&document, "transport") == MANAGED_TRANSPORT
            && string(&document, "runtime") == "claude"
            && string(&document, "provider_thread_id").is_empty()
            && valid_session_id(&string(&document, "provider_session_id"));
        if !valid {
            return Ok(None);
        }
        // A known transcript conflict is an authority fence, not configuration
        // drift. Only explicit New Session or validated retarget may retire it.
        identity_conflict::require_clear(&document)?;
        let selected = string(&document, "explicit_resume_session_id")
            == string(&document, "provider_session_id");
        let saved_cwd = std::path::PathBuf::from(string(&document, "workspace_path"));
        if workspace_path(cwd) != workspace_path(&saved_cwd) {
            return incompatible_receipt(
                home,
                group_id,
                actor_id,
                &mut document,
                selected,
                "workspace_path",
            );
        }
        let stripped = continuity::without_model(base_command);
        let legacy = continuity::prior_command(base_command, &string(&document, "model"));
        let exact_command =
            string(&document, "command_fingerprint") == command_fingerprint(base_command);
        let normalized_command = string(&document, "command_without_model_fingerprint")
            == command_fingerprint(&stripped);
        let legacy_command =
            string(&document, "command_fingerprint") == command_fingerprint(&legacy);
        if !(exact_command || normalized_command || legacy_command) {
            return incompatible_receipt(
                home,
                group_id,
                actor_id,
                &mut document,
                selected,
                "command_fingerprint",
            );
        }
        let identity = identity_fingerprint(&stripped, environment, cwd)?;
        let exact_identity = string(&document, "identity_fingerprint")
            == identity_fingerprint(base_command, environment, cwd)?;
        let normalized_identity =
            string(&document, "identity_without_model_fingerprint") == identity;
        let legacy_identity = legacy_command
            && string(&document, "identity_fingerprint")
                == identity_fingerprint(&legacy, environment, cwd)?;
        if !(exact_identity || normalized_identity || legacy_identity) {
            return incompatible_receipt(
                home,
                group_id,
                actor_id,
                &mut document,
                selected,
                "identity_fingerprint",
            );
        }
        identity_conflict::observe(home, group_id, actor_id, cwd, environment, &mut document)?;
        let model = model_from_command(base_command);
        let model_change = (model != string(&document, "model")).then_some(model.clone());
        if string(&document, "status") == "resume_failed" {
            // A new ordinary configured identity retains upstream fresh-start
            // behavior. An unchanged failed attempt remains blocked.
            let failed_model = document
                .get("last_failed_resume_model")
                .and_then(Value::as_str)
                .unwrap_or_else(|| document.get("model").and_then(Value::as_str).unwrap_or(""));
            if model != failed_model && !selected {
                return Ok(None);
            }
            let reason = string(&document, "last_resume_error");
            if let Err(error) = recovery_decision(
                home,
                group_id,
                actor_id,
                &mut document,
                "block",
                "previous_resume_failure",
            ) {
                tracing::warn!(%error, group_id = %group_id, actor_id = %actor_id,
                    "optional runtime recovery diagnostic was not persisted");
            }
            return Err(io::Error::other(ResumeBlocked(reason)));
        }
        if string(&document, "status") != "usable"
            || document.get("resume_eligible").and_then(Value::as_bool) != Some(true)
        {
            return incompatible_receipt(
                home,
                group_id,
                actor_id,
                &mut document,
                selected,
                "resume_eligibility",
            );
        }
        let session_id = string(&document, "provider_session_id");
        let attempt_id = uuid::Uuid::new_v4().simple().to_string();
        document.insert("last_resume_attempt_at".into(), json!(utc_now()));
        document.insert("last_resume_attempt_id".into(), json!(attempt_id));
        document.insert("last_resume_attempt_model".into(), json!(model));
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
    })
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

fn incompatible_receipt(
    home: &HomeLayout,
    group: &str,
    actor: &str,
    document: &mut Map<String, Value>,
    selected: bool,
    field: &str,
) -> io::Result<Option<ResumeAttempt>> {
    if selected {
        continuity::blocked(home, group, actor, document, field)
    } else {
        // Configuration drift before an implicit attempt chooses a fresh
        // session; retain the old binding until that launch succeeds.
        Ok(None)
    }
}

/// Retire only the exact attempt which could not begin a model transition.
/// This remains an authoritative fence write, unlike recovery observations.
pub fn abandon_resume_attempt(
    home: &HomeLayout,
    group: &str,
    actor: &str,
    attempt: &ResumeAttempt,
) -> io::Result<()> {
    super::with_receipt_lock(home, group, actor, || {
        let mut document = read(home, group, actor)?;
        identity_conflict::require_clear(&document)?;
        if string(&document, "provider_session_id") == attempt.session_id
            && string(&document, "last_resume_attempt_id") == attempt.attempt_id
        {
            document.insert("last_resume_attempt_id".into(), json!(""));
            document.insert("updated_at".into(), json!(utc_now()));
            write(home, group, actor, &document)?;
        } else {
            return Err(io::Error::other(
                "Claude resume binding changed before the fresh fallback; launch was refused",
            ));
        }
        Ok(())
    })
}

pub fn archive_new_session(home: &HomeLayout, group: &str, actor: &str) -> io::Result<()> {
    super::with_receipt_lock(home, group, actor, || {
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
        document.remove("explicit_resume_session_id");
        document.remove("explicit_resume_pending");
        document.remove("claude_identity_conflict");
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
    })
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
    super::with_receipt_lock(home, group_id, actor_id, || {
        let mut document = read(home, group_id, actor_id)?;
        if string(&document, "provider_session_id") != attempt.session_id
            || string(&document, "last_resume_attempt_id") != attempt.attempt_id
        {
            return Ok(());
        }
        document.insert("last_resume_error".into(), json!(diagnostic));
        let attempted_model = document
            .get("last_resume_attempt_model")
            .cloned()
            .unwrap_or_else(|| json!(string(&document, "model")));
        document.insert("last_failed_resume_model".into(), attempted_model);
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
    })
}

/// Called only by an authorized explicit Actor lifecycle action, never auto-wake or polling.
#[cfg(test)]
pub fn retry_failed_resume(home: &HomeLayout, group_id: &str, actor_id: &str) -> io::Result<bool> {
    retry_failed_resume_matching(home, group_id, actor_id, None)
}

pub fn retry_failed_resume_for_command(
    home: &HomeLayout,
    group_id: &str,
    actor_id: &str,
    command: &[String],
) -> io::Result<bool> {
    let model = model_from_command(command);
    retry_failed_resume_matching(home, group_id, actor_id, Some(&model))
}

fn retry_failed_resume_matching(
    home: &HomeLayout,
    group_id: &str,
    actor_id: &str,
    model: Option<&str>,
) -> io::Result<bool> {
    if !resume_enabled() {
        return Ok(false);
    }
    with_optional_resume_lock(home, group_id, actor_id, false, || {
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
        let selected = string(&document, "explicit_resume_session_id")
            == string(&document, "provider_session_id");
        let failed_model = document
            .get("last_failed_resume_model")
            .and_then(Value::as_str)
            .unwrap_or_else(|| document.get("model").and_then(Value::as_str).unwrap_or(""));
        if !selected && model.is_some_and(|model| model != failed_model) {
            // Preserve failed-attempt evidence until prepare can select the
            // upstream fresh path for genuinely changed ordinary settings.
            return Ok(false);
        }
        document.insert("status".into(), json!("usable"));
        document.insert("resume_eligible".into(), json!(true));
        document.insert("updated_at".into(), json!(utc_now()));
        write(home, group_id, actor_id, &document)?;
        Ok(true)
    })
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
    super::with_receipt_lock(home, group_id, actor_id, || {
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
        // A delayed successful callback cannot erase a later conflict or
        // replace a different conversation selected by a newer transaction.
        identity_conflict::require_clear(&old)?;
        if resumed
            && valid_session_id(&string(&old, "provider_session_id"))
            && string(&old, "provider_session_id") != session_id
        {
            return Err(io::Error::other(ResumeBlocked(
                "Claude resume binding changed before success was recorded".into(),
            )));
        }
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
        if resumed && string(&old, "provider_session_id") == session_id {
            retarget::retain_model(&old, &mut document, session_id);
            if string(&old, "explicit_resume_session_id") == session_id {
                document.insert("explicit_resume_session_id".into(), json!(session_id));
            }
        }
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
    })
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
            .expect("changed identity")
            .is_none()
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
            .expect("changed provider configuration")
            .is_none()
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
            .expect("changed input")
            .is_none()
        );
    }
}

#[cfg(test)]
#[path = "claude_lock_acceptance_tests.rs"]
mod lock_acceptance_tests;
