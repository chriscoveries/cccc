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
fn continuity_account_drift_starts_fresh_without_rewriting_the_saved_session() {
    let (_temp, home, group, cwd, command, mut env) = fixture();
    record_managed(&home, &group, "actor", &cwd, &command, &env, SAVED, false)
        .expect("fixture operation");
    let before = read(&home, &group, "actor").expect("saved receipt");
    env.insert("CLAUDE_CONFIG_DIR".into(), "/different-account".into());
    assert!(
        prepare_managed(&home, &group, "actor", &cwd, &command, &env)
            .expect("different configured identity chooses fresh")
            .is_none()
    );
    assert_eq!(read(&home, &group, "actor").expect("saved receipt"), before);
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
fn changed_workspace_chooses_current_directory_for_ordinary_and_explicit_start() {
    let (temp, home, group, cwd, command, env) = fixture();
    record_managed(&home, &group, "actor", &cwd, &command, &env, SAVED, false)
        .expect("saved receipt");
    let before = read(&home, &group, "actor").expect("saved receipt");
    let other = temp.path().join("other");
    std::fs::create_dir(&other).expect("current workspace");
    for explicit in [false, true] {
        assert!(
            prepare_managed_with_policy(&home, &group, "actor", &other, &command, &env, explicit)
                .expect("new workspace chooses a fresh session")
                .is_none()
        );
        assert_eq!(read(&home, &group, "actor").expect("saved receipt"), before);
    }
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
    let before = read(&home, &group, "actor").expect("saved receipt");
    assert!(
        prepare_managed(&home, &group, "actor", &cwd, &command, &env)
            .expect("changed effort is a different configured launch")
            .is_none()
    );
    assert_eq!(read(&home, &group, "actor").expect("saved receipt"), before);
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

#[test]
fn optional_recovery_diagnostics_do_not_create_or_overwrite_corrupt_receipts() {
    let (_temp, home, group, _cwd, _command, _env) = fixture();
    let receipt = home
        .groups_dir()
        .join(&group)
        .join("state/runtime_sessions/actor.json");
    crate::ops::runtime_session::recovery_decision(
        &home,
        &group,
        "actor",
        "skip",
        "actor_disabled",
    )
    .expect("optional missing-receipt observation");
    assert!(
        !receipt.exists(),
        "observation must not manufacture a receipt"
    );
    std::fs::create_dir_all(receipt.parent().expect("receipt parent")).expect("receipt directory");
    let corrupt = b"{not-valid-json";
    std::fs::write(&receipt, corrupt).expect("corrupt receipt fixture");
    crate::ops::runtime_session::recovery_decision(
        &home,
        &group,
        "actor",
        "skip",
        "actor_disabled",
    )
    .expect("optional corrupt-receipt observation");
    assert_eq!(
        std::fs::read(&receipt).expect("retained corrupt receipt"),
        corrupt
    );
}

#[test]
fn provider_receipt_commits_even_when_the_optional_recovery_ledger_is_unwritable() {
    let (_temp, home, group, cwd, command, env) = fixture();
    let ledger = GroupStore::new(home.clone())
        .expect("store")
        .ledger_path(&group)
        .expect("ledger");
    if ledger.is_file() {
        std::fs::remove_file(&ledger).expect("owned empty fixture ledger");
    }
    std::fs::create_dir_all(&ledger).expect("inject directory-at-ledger failure");
    record_managed(&home, &group, "actor", &cwd, &command, &env, SAVED, false)
        .expect("authoritative receipt success cannot become a diagnostic failure");
    let receipt = read(&home, &group, "actor").expect("committed provider receipt");
    assert_eq!(receipt["provider_session_id"], SAVED);
    assert_eq!(receipt["status"], "usable");
    assert!(
        ledger.is_dir(),
        "do not replace the injected failed diagnostic sink"
    );
}

#[test]
fn repeated_recovery_observation_emits_only_one_state_change() {
    let (_temp, home, group, cwd, command, env) = fixture();
    record_managed(&home, &group, "actor", &cwd, &command, &env, SAVED, false).expect("receipt");
    for _ in 0..6 {
        crate::ops::runtime_session::recovery_decision(
            &home,
            &group,
            "actor",
            "skip",
            "already_running",
        )
        .expect("optional observation");
    }
    let events = cccc_core::ledger::read_all(
        &GroupStore::new(home)
            .expect("store")
            .ledger_path(&group)
            .expect("ledger"),
    )
    .expect("recovery events");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == "runtime.session.recovery"
                && event.data["action"] == "skip"
                && event.data["reason"] == "already_running")
            .count(),
        1
    );
}
