use super::*;
use cccc_core::GroupStore;

fn fixture() -> (
    tempfile::TempDir,
    HomeLayout,
    String,
    std::path::PathBuf,
    Vec<String>,
    BTreeMap<String, String>,
) {
    let temp = tempfile::tempdir().expect("fixture operation");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("fixture operation");
    let group = GroupStore::new(home.clone())
        .expect("fixture operation")
        .create("continuity", "")
        .expect("fixture operation")
        .group_id;
    let cwd = temp.path().join("worktree");
    std::fs::create_dir(&cwd).expect("fixture operation");
    (
        temp,
        home,
        group,
        cwd,
        vec![
            "claude".into(),
            "--model".into(),
            "sonnet".into(),
            "--effort".into(),
            "low".into(),
        ],
        BTreeMap::new(),
    )
}
const SAVED: &str = "52b41c61-e23c-4b7c-8b60-809c347451b5";

#[test]
fn continuity_model_swap_keeps_the_saved_session_pointer() {
    let (_temp, home, group, cwd, mut command, env) = fixture();
    record_managed(&home, &group, "actor", &cwd, &command, &env, SAVED, false)
        .expect("fixture operation");
    command[2] = "opus".into();
    let attempt = prepare_managed(&home, &group, "actor", &cwd, &command, &env)
        .expect("fixture operation")
        .expect("model swap must resume");
    assert_eq!(attempt.session_id, SAVED);
}
#[test]
fn continuity_account_drift_blocks_and_names_the_saved_session() {
    let (_temp, home, group, cwd, command, mut env) = fixture();
    record_managed(&home, &group, "actor", &cwd, &command, &env, SAVED, false)
        .expect("fixture operation");
    env.insert("CLAUDE_CONFIG_DIR".into(), "/different-account".into());
    let error = match prepare_managed(&home, &group, "actor", &cwd, &command, &env) {
        Err(error) => error,
        Ok(_) => panic!("account drift must block"),
    };
    assert!(error.to_string().contains(SAVED));
    assert!(error.to_string().contains("identity_fingerprint"));
    assert_eq!(
        read(&home, &group, "actor").expect("fixture operation")["provider_session_id"],
        SAVED
    );
}
#[test]
fn continuity_replacement_archives_the_old_pointer() {
    let (_temp, home, group, cwd, command, env) = fixture();
    record_managed(&home, &group, "actor", &cwd, &command, &env, SAVED, false)
        .expect("fixture operation");
    record_managed(
        &home,
        &group,
        "actor",
        &cwd,
        &command,
        &env,
        "52b41c61-e23c-4b7c-8b60-809c347451b6",
        false,
    )
    .expect("fixture operation");
    let stored = read(&home, &group, "actor").expect("fixture operation");
    assert_eq!(stored["previous_sessions"][0]["session_id"], SAVED);
    assert_eq!(
        stored["previous_sessions"][0]["workspace_path"],
        workspace_path(&cwd)
    );
}

#[test]
fn automatic_workspace_drift_blocks_but_explicit_recovery_uses_saved_workspace() {
    let (temp, home, group, cwd, command, env) = fixture();
    record_managed(&home, &group, "actor", &cwd, &command, &env, SAVED, false)
        .expect("fixture operation");
    let other = temp.path().join("other");
    std::fs::create_dir(&other).expect("fixture operation");
    assert_eq!(
        explicit_workspace(&home, &group, "actor").expect("fixture operation"),
        Some(cwd.clone())
    );
    let attempt = prepare_managed_with_policy(&home, &group, "actor", &cwd, &command, &env, true)
        .expect("fixture operation")
        .expect("fixture operation");
    assert_eq!(attempt.session_id, SAVED);
    let error = prepare_managed(&home, &group, "actor", &other, &command, &env)
        .err()
        .expect("fixture operation");
    assert!(is_resume_blocked(&error));
    assert!(error.to_string().contains("workspace_path"));
    assert_eq!(
        read(&home, &group, "actor").expect("fixture operation")["provider_session_id"],
        SAVED
    );
}

#[test]
fn explicit_new_session_archives_before_retiring_pointer_and_preserves_history() {
    let (_temp, home, group, cwd, command, env) = fixture();
    record_managed(&home, &group, "actor", &cwd, &command, &env, SAVED, false)
        .expect("fixture operation");
    archive_new_session(&home, &group, "actor").expect("fixture operation");
    let retired = read(&home, &group, "actor").expect("fixture operation");
    assert_eq!(retired["provider_session_id"], "");
    assert_eq!(retired["previous_sessions"][0]["session_id"], SAVED);
    assert_eq!(retired["previous_sessions"][0]["reason"], "new_session");
    archive_new_session(&home, &group, "actor").expect("fixture operation");
    record_managed(
        &home,
        &group,
        "actor",
        &cwd,
        &command,
        &env,
        "52b41c61-e23c-4b7c-8b60-809c347451b6",
        false,
    )
    .expect("fixture operation");
    let next = read(&home, &group, "actor").expect("fixture operation");
    assert_eq!(
        next["previous_sessions"]
            .as_array()
            .expect("fixture operation")
            .len(),
        1
    );
    assert_eq!(next["previous_sessions"][0]["session_id"], SAVED);
}

#[test]
fn legacy_receipt_accepts_only_model_drift_and_still_checks_account() {
    let (_temp, home, group, cwd, mut command, env) = fixture();
    record_managed(&home, &group, "actor", &cwd, &command, &env, SAVED, false)
        .expect("fixture operation");
    let mut legacy = read(&home, &group, "actor").expect("fixture operation");
    legacy.remove("command_without_model_fingerprint");
    legacy.remove("identity_without_model_fingerprint");
    write(&home, &group, "actor", &legacy).expect("fixture operation");
    command[2] = "opus".into();
    assert_eq!(
        prepare_managed(&home, &group, "actor", &cwd, &command, &env)
            .expect("fixture operation")
            .expect("fixture operation")
            .session_id,
        SAVED
    );
    command[4] = "high".into();
    let error = prepare_managed(&home, &group, "actor", &cwd, &command, &env)
        .err()
        .expect("fixture operation");
    assert!(error.to_string().contains("command_fingerprint"));
}

#[test]
fn replacement_history_is_bounded_and_keeps_most_recent_twenty() {
    let (_temp, home, group, cwd, command, env) = fixture();
    let mut ids = Vec::new();
    for _ in 0..25 {
        let id = uuid::Uuid::new_v4().to_string();
        record_managed(&home, &group, "actor", &cwd, &command, &env, &id, false)
            .expect("fixture operation");
        ids.push(id);
    }
    let receipt = read(&home, &group, "actor").expect("fixture operation");
    let history = receipt["previous_sessions"]
        .as_array()
        .expect("fixture operation");
    assert_eq!(history.len(), 20);
    assert_eq!(history[0]["session_id"], ids[4]);
    assert_eq!(history[19]["session_id"], ids[23]);
    assert_eq!(receipt["provider_session_id"], ids[24]);
}

#[test]
fn failed_managed_start_records_visible_reason_without_retiring_saved_thread() {
    let (_temp, home, group, cwd, command, env) = fixture();
    let runtime = crate::ops::runtime_session::CodexAppThread {
        id: "saved-thread",
        resumed: false,
    };
    crate::ops::runtime_session::record_codex_app_thread(
        &home, &group, "actor", &cwd, &command, &env, runtime,
    )
    .expect("fixture operation");
    crate::ops::runtime_session::recovery_decision(
        &home,
        &group,
        "actor",
        "attempt",
        "automatic_start",
    )
    .expect("fixture operation");
    crate::ops::runtime_session::recovery_decision(
        &home,
        &group,
        "actor",
        "failure",
        "managed startup did not yield a started session (TimedOut)",
    )
    .expect("fixture operation");
    let receipt = read(&home, &group, "actor").expect("fixture operation");
    assert_eq!(receipt["provider_thread_id"], "saved-thread");
    assert!(
        receipt["last_resume_error"]
            .as_str()
            .expect("fixture operation")
            .contains("TimedOut")
    );
    assert_eq!(receipt["last_recovery_decision"]["action"], "failure");
    let events = std::fs::read_to_string(
        GroupStore::new(home.clone())
            .expect("fixture operation")
            .ledger_path(&group)
            .expect("fixture operation"),
    )
    .expect("fixture operation");
    assert!(
        events
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .any(|event| event["kind"] == "runtime.session.recovery"
                && event["data"]["action"] == "failure")
    );
}
