//! Persist observed worker/conversation divergence without implicitly adopting history.

use super::*;
use cccc_core::GroupStore;

const WORKER_W: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaa1";
const TRANSCRIPT_S: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbb2";
const SHORT_W: &str = "a1b2c3d4";
const ACTOR: &str = "claude-1";

/// Both choices on ONE line — asserted in the durable blocked diagnostic.
const CHOICE_A: &str = "adopts that conversation";
const CHOICE_B: &str = "New Session";

struct Fixture {
    _temp: tempfile::TempDir,
    home: HomeLayout,
    group: String,
    cwd: std::path::PathBuf,
    config_dir: std::path::PathBuf,
    command: Vec<String>,
    env: BTreeMap<String, String>,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("fixture tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("fixture home");
        home.initialize().expect("fixture home init");
        let group = GroupStore::new(home.clone())
            .expect("fixture store")
            .create("identity receipt v2", "")
            .expect("fixture group")
            .group_id;
        let cwd = temp.path().join("workspace");
        std::fs::create_dir(&cwd).expect("fixture workspace");
        let config_dir = temp.path().join("claude-config");
        std::fs::create_dir_all(config_dir.join("jobs").join(SHORT_W)).expect("fixture jobs dir");
        std::fs::create_dir_all(config_dir.join("projects").join("workspace"))
            .expect("fixture projects dir");
        let command = vec!["claude".into()];
        let env = BTreeMap::from([(
            "CLAUDE_CONFIG_DIR".into(),
            config_dir.to_string_lossy().into_owned(),
        )]);
        Self {
            _temp: temp,
            home,
            group,
            cwd,
            config_dir,
            command,
            env,
        }
    }

    fn state_path(&self) -> std::path::PathBuf {
        self.config_dir
            .join("jobs")
            .join(SHORT_W)
            .join("state.json")
    }

    fn transcript_path(&self) -> std::path::PathBuf {
        self.config_dir
            .join("projects")
            .join("workspace")
            .join(format!("{TRANSCRIPT_S}.jsonl"))
    }

    /// Positively observed W→S: worker W's OWN job state (same short, same
    /// cwd) carries resumeSessionId=S + linkScanPath=S, and the contained S
    /// transcript record carries cwd=current cwd. Observation only — never
    /// authority by itself.
    fn write_adopting_observation(&self) {
        let transcript = self.transcript_path();
        let cwd_str = self.cwd.to_string_lossy().into_owned();
        std::fs::write(
            &transcript,
            serde_json::json!({
                "type": "custom-title",
                "sessionId": TRANSCRIPT_S,
                "cwd": cwd_str,
            })
            .to_string()
                + "\n",
        )
        .expect("fixture S transcript");
        let state = serde_json::json!({
            "sessionId": WORKER_W,
            "daemonShort": SHORT_W,
            "resumeSessionId": TRANSCRIPT_S,
            "linkScanPath": transcript,
            "tempo": "idle",
            "cwd": cwd_str,
        });
        cccc_core::fs::write_json(&self.state_path(), &state).expect("fixture state");
    }

    fn record_worker(&self) {
        record_managed(
            &self.home,
            &self.group,
            ACTOR,
            &self.cwd,
            &self.command,
            &self.env,
            WORKER_W,
            false,
        )
        .expect("record worker W");
    }

    fn prepare(&self) -> io::Result<Option<ResumeAttempt>> {
        prepare_managed(
            &self.home,
            &self.group,
            ACTOR,
            &self.cwd,
            &self.command,
            &self.env,
        )
    }

    fn receipt(&self) -> Map<String, Value> {
        read(&self.home, &self.group, ACTOR).expect("receipt")
    }
}

/// External durability: receipt durably records BOTH W and S without insisting
/// on one exact field name; if the planned `claude_identity_conflict` shape is
/// present, its fields are verified strictly.
fn assert_durable_w_and_s(stored: &Map<String, Value>) {
    let text = serde_json::to_string(stored).expect("receipt serializes");
    assert!(text.contains(WORKER_W), "W durably recorded: {text}");
    assert!(text.contains(TRANSCRIPT_S), "S durably recorded: {text}");
    if let Some(conflict) = stored.get("claude_identity_conflict") {
        assert_eq!(
            conflict["worker_session_id"], WORKER_W,
            "conflict worker: {conflict}"
        );
        let observed = conflict["observed_transcripts"]
            .as_array()
            .expect("observed_transcripts array");
        assert!(
            observed
                .iter()
                .any(|entry| entry["session_id"] == TRANSCRIPT_S),
            "conflict observes S: {conflict}"
        );
        let entry = observed
            .iter()
            .find(|entry| entry["session_id"] == TRANSCRIPT_S)
            .expect("S entry");
        assert_eq!(entry["daemon_short"], SHORT_W, "conflict short: {entry}");
        let path = entry["transcript_path"].as_str().expect("transcript path");
        assert!(
            path.contains(TRANSCRIPT_S),
            "conflict path names S: {entry}"
        );
    }
}

fn assert_block_choices(text: &str) {
    assert!(
        text.contains(&format!("Resume {TRANSCRIPT_S} (adopts that conversation)")),
        "block names exact adoption choice: {text}"
    );
    assert!(text.contains(CHOICE_B), "block names New Session: {text}");
    let (a_line, b_line) = (
        text.lines().find(|line| line.contains(CHOICE_A)),
        text.lines().find(|line| line.contains(CHOICE_B)),
    );
    assert_eq!(a_line, b_line, "both choices share one line: {text}");
}

#[test]
fn first_prepare_on_observed_w_to_s_blocks_and_records_both() {
    let f = Fixture::new();
    f.record_worker();
    f.write_adopting_observation();
    let before = f.receipt();
    assert_eq!(before["provider_session_id"], WORKER_W);
    assert_eq!(before["status"], "usable");

    // NO manual failure recording anywhere in this file: discovery must happen
    // inside prepare itself. On old872 this returns Ok(SomeW) and the test
    // fails here (red) by design.
    let error = f
        .prepare()
        .err()
        .expect("poisoned W→S must block, never SomeW");
    assert!(
        is_resume_blocked(&error),
        "durable block, not fresh: {error}"
    );
    assert_block_choices(&error.to_string());

    let stored = f.receipt();
    assert_durable_w_and_s(&stored);
    assert_eq!(stored["provider_session_id"], WORKER_W, "canonical stays W");
    assert_eq!(stored["resume_eligible"], false);
    assert!(
        stored["status"] == "identity_conflict",
        "conflict status (planned shape): {}",
        stored["status"]
    );
    // Identity discovery before execution: no execution-failure accounting.
    assert_eq!(
        stored["failure_count"], before["failure_count"],
        "discovery mints no failure count"
    );
    assert_eq!(
        stored["last_resume_attempt_id"], before["last_resume_attempt_id"],
        "discovery mints no attempt"
    );
    assert!(f.transcript_path().exists(), "S evidence preserved");
}

#[test]
fn repeated_blocked_prepare_never_launches_resets_or_remints() {
    let f = Fixture::new();
    f.record_worker();
    f.write_adopting_observation();
    f.prepare().err().expect("first prepare blocks");
    let frozen = f.receipt();

    for _ in 0..4 {
        let error = f.prepare().err().expect("still blocked, not fresh");
        assert!(is_resume_blocked(&error));
        assert_block_choices(&error.to_string());
    }
    let mut after = f.receipt();
    assert_eq!(
        after["provider_session_id"], WORKER_W,
        "no fresh pointer minted"
    );
    assert_eq!(after["resume_eligible"], false);
    assert_eq!(
        after["failure_count"], frozen["failure_count"],
        "no execution-failure reset"
    );
    assert_eq!(
        after["last_resume_attempt_id"], frozen["last_resume_attempt_id"],
        "no reminted attempt"
    );
    for key in ["updated_at", "last_recovery_decision"] {
        after.remove(key);
    }
    let mut expected = frozen.clone();
    for key in ["updated_at", "last_recovery_decision"] {
        expected.remove(key);
    }
    assert_eq!(after, expected, "blocked polls manufacture nothing");
}

#[test]
fn evidence_symlink_missing_conflicting_cannot_auto_resume_or_fresh() {
    let f = Fixture::new();
    f.record_worker();
    f.write_adopting_observation();
    f.prepare().err().expect("first prepare blocks");

    let state_path = f.state_path();
    let transcript = f.transcript_path();

    #[cfg(unix)]
    {
        let real = state_path.with_extension("json.real");
        std::fs::rename(&state_path, &real).expect("stash state");
        std::os::unix::fs::symlink(&real, &state_path).expect("symlink state");
        let error = f.prepare().err().expect("symlink evidence stays blocked");
        assert!(is_resume_blocked(&error));
        std::fs::remove_file(&state_path).expect("remove symlink");
        std::fs::rename(&real, &state_path).expect("restore state");
    }

    // Missing S transcript behind a positively observed W→S is still a
    // blocked, unvalidated observation — never silent fresh.
    let held = transcript.with_extension("jsonl.held");
    std::fs::rename(&transcript, &held).expect("stash transcript");
    let error = f.prepare().err().expect("missing S stays blocked");
    assert!(is_resume_blocked(&error));
    assert_eq!(f.receipt()["provider_session_id"], WORKER_W);
    std::fs::rename(&held, &transcript).expect("restore transcript");

    // Unknown missing worker state alone is not proof of conflict — but here
    // the receipt already holds a durable conflict, so absence changes nothing.
    std::fs::remove_file(&state_path).expect("remove worker state");
    let error = f
        .prepare()
        .err()
        .expect("durable conflict survives state loss");
    assert!(is_resume_blocked(&error));
}

#[test]
fn explicit_new_session_retires_conflict_and_preserves_lineage() {
    let f = Fixture::new();
    f.record_worker();
    f.write_adopting_observation();
    f.prepare().err().expect("first prepare blocks");

    archive_new_session(&f.home, &f.group, ACTOR).expect("new session");
    let retired = f.receipt();
    assert_eq!(retired["provider_session_id"], "");
    assert_eq!(retired["resume_eligible"], false);
    assert!(
        retired.get("claude_identity_conflict").is_none(),
        "current conflict removed: {retired:?}"
    );
    let history = retired["previous_sessions"]
        .as_array()
        .expect("lineage history");
    let history_text = serde_json::to_string(history).expect("history serializes");
    assert!(
        history_text.contains(WORKER_W),
        "W lineage archived: {history_text}"
    );
    assert!(
        history_text.contains(TRANSCRIPT_S),
        "observed S lineage archived: {history_text}"
    );
    assert!(f.transcript_path().exists(), "S evidence preserved on disk");
    assert!(
        f.prepare()
            .expect("chooser after explicit new session")
            .is_none(),
        "fresh only because the operator chose New Session"
    );
}

#[test]
fn stale_success_must_not_erase_durable_conflict() {
    let f = Fixture::new();
    f.record_worker();
    f.write_adopting_observation();
    f.prepare().err().expect("first prepare blocks");

    // A late success callback for the stale worker must not reopen the receipt.
    record_managed(
        &f.home, &f.group, ACTOR, &f.cwd, &f.command, &f.env, WORKER_W, true,
    )
    .expect_err("a stale success cannot erase an authoritative conflict");
    let stored = f.receipt();
    let text = serde_json::to_string(&stored).expect("receipt serializes");
    assert!(
        text.contains(TRANSCRIPT_S),
        "conflict survives stale success: {text}"
    );
    assert_eq!(stored["resume_eligible"], false, "still fenced: {text}");
    let error = f
        .prepare()
        .err()
        .expect("still blocked after stale success");
    assert!(is_resume_blocked(&error));
}

#[test]
fn valid_exact_w_control_still_prepares_w() {
    let f = Fixture::new();
    f.record_worker();
    // No adopting observation: exact configured identity resumes (passes old872 too).
    let attempt = f.prepare().expect("prepare").expect("exact W resumes");
    assert_eq!(attempt.session_id, WORKER_W);
    assert_eq!(f.receipt()["status"], "usable");
}

#[test]
fn positive_adoption_with_missing_history_is_durably_unvalidated_and_blocked() {
    let f = Fixture::new();
    f.record_worker();
    f.write_adopting_observation();
    std::fs::remove_file(f.transcript_path()).expect("remove only the owned fixture transcript");
    let error = f
        .prepare()
        .err()
        .expect("reported foreign identity cannot be resumed");
    assert!(is_resume_blocked(&error));
    assert_block_choices(&error.to_string());
    let receipt = f.receipt();
    let observed = &receipt["claude_identity_conflict"]["observed_transcripts"][0];
    assert_eq!(observed["session_id"], TRANSCRIPT_S);
    assert_eq!(observed["transcript_validated"], false);
    assert!(observed["validation_error"].is_string());
    assert_eq!(receipt["provider_session_id"], WORKER_W);
    assert_eq!(receipt["last_resume_attempt_id"], "");
}

#[test]
fn known_conflict_blocks_configuration_drift_without_rewriting_evidence() {
    let mut f = Fixture::new();
    f.record_worker();
    f.write_adopting_observation();
    f.prepare().err().expect("discover conflict");
    let before = f.receipt();
    f.env.insert(
        "CLAUDE_CONFIG_DIR".into(),
        f.cwd.join("another-account").to_string_lossy().into_owned(),
    );
    f.command.extend(["--model".into(), "opus".into()]);
    let error = f
        .prepare()
        .err()
        .expect("known conflict is not an implicit fresh request");
    assert!(is_resume_blocked(&error));
    assert_block_choices(&error.to_string());
    assert_eq!(f.receipt(), before);
}

#[test]
fn explicit_retarget_archives_conflict_and_retains_selected_conversation_on_reopen() {
    let mut f = Fixture::new();
    f.command.extend(["--model".into(), "sonnet".into()]);
    f.record_worker();
    f.write_adopting_observation();
    use std::io::Write;
    let mut transcript = std::fs::OpenOptions::new()
        .append(true)
        .open(f.transcript_path())
        .expect("owned transcript");
    writeln!(transcript, "{}", json!({"type":"assistant", "sessionId":TRANSCRIPT_S,"cwd":f.cwd,"message":{"model":"haiku"}})).expect("retained model");
    drop(transcript);
    f.prepare().err().expect("discover conflict");
    let selected = validate_retarget(
        &f.home,
        &f.group,
        ACTOR,
        &f.cwd,
        &f.command,
        &f.env,
        TRANSCRIPT_S,
    )
    .expect("explicitly select validated non-live history");
    write_retarget(&f.home, &f.group, ACTOR, &selected.target).expect("operator retarget commit");
    let receipt = f.receipt();
    assert_eq!(receipt["provider_session_id"], TRANSCRIPT_S);
    assert!(receipt.get("claude_identity_conflict").is_none());
    let history = receipt["previous_sessions"]
        .as_array()
        .expect("retarget lineage");
    assert!(history.iter().any(|entry| entry["session_id"] == WORKER_W
        && entry["claude_identity_conflict"]["worker_session_id"] == WORKER_W));
    record_managed(
        &f.home,
        &f.group,
        ACTOR,
        &f.cwd,
        &f.command,
        &f.env,
        TRANSCRIPT_S,
        true,
    )
    .expect("successful explicit resume");
    // A new HomeLayout reads only persisted state. This is not a native daemon restart.
    let reopened = HomeLayout::from_path(f._temp.path().join("home")).expect("reopened home");
    let stored = read(&reopened, &f.group, ACTOR).expect("durable selected receipt");
    assert_eq!(stored["provider_session_id"], TRANSCRIPT_S);
    assert_eq!(stored["explicit_resume_session_id"], TRANSCRIPT_S);
    assert_ne!(
        stored
            .get("explicit_resume_pending")
            .and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(stored["model"], "haiku");
    let attempt = prepare_managed(&reopened, &f.group, ACTOR, &f.cwd, &f.command, &f.env)
        .expect("prepare retained choice")
        .expect("selected S resumes");
    assert_eq!(attempt.session_id, TRANSCRIPT_S);
}

#[test]
fn explicit_retarget_cannot_clear_a_conflict_by_reselecting_the_poisoned_worker() {
    let f = Fixture::new();
    f.record_worker();
    f.write_adopting_observation();
    f.prepare().err().expect("discover conflict");
    let before = f.receipt();
    let error = match validate_retarget(
        &f.home, &f.group, ACTOR, &f.cwd, &f.command, &f.env, WORKER_W,
    ) {
        Ok(_) => panic!("the poisoned worker is not an authorized conversation choice"),
        Err(error) => error,
    };
    assert_eq!(error.code, "claude_identity_conflict");
    assert_block_choices(&error.message);
    assert_eq!(f.receipt(), before);
}

#[test]
fn known_conflict_stays_blocked_with_resume_disabled() {
    // Isolate process-global resume configuration in a child test process.
    if let Ok(root) = std::env::var("CCCC_TEST_IDENTITY_RECEIPT_ROOT") {
        let root = std::path::PathBuf::from(root);
        let home = HomeLayout::from_path(root.join("home")).expect("existing receipt home");
        let group = std::env::var("CCCC_TEST_IDENTITY_RECEIPT_GROUP").expect("owned group");
        let env = BTreeMap::from([(
            "CLAUDE_CONFIG_DIR".into(),
            root.join("claude-config").to_string_lossy().into_owned(),
        )]);
        let error = prepare_managed(
            &home,
            &group,
            ACTOR,
            &root.join("workspace"),
            &["claude".into()],
            &env,
        )
        .err()
        .expect("resume-off does not authorize dropping a known conflict");
        assert!(is_resume_blocked(&error));
        assert_block_choices(&error.to_string());
        return;
    }
    let f = Fixture::new();
    f.record_worker();
    f.write_adopting_observation();
    f.prepare()
        .err()
        .expect("discover conflict before disabling normal resume");
    let before = f.receipt();
    let child = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", "ops::runtime_session::claude::identity_receipt_tests::known_conflict_stays_blocked_with_resume_disabled", "--nocapture"])
        .env("CCCC_RUNTIME_RESUME", "0")
        .env("CCCC_TEST_IDENTITY_RECEIPT_ROOT", f._temp.path())
        .env("CCCC_TEST_IDENTITY_RECEIPT_GROUP", &f.group)
        .output().expect("isolated resume-off test");
    assert!(
        child.status.success(),
        "resume-off control failed: {} {}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
    assert_eq!(f.receipt(), before);
}
