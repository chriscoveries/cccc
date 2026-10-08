//! Exercise real Actor lifecycle/delivery against a local Agent View fixture, no provider tasks.
use cccc_contracts::{Actor, ActorRuntime, DaemonRequest, Event};
use cccc_core::{GroupDoc, GroupStore, HomeLayout, ledger};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    process::Child,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};

const SAVED: &str = "52b41c61-e23c-4b7c-8b60-809c347451b5";

fn isolated(test: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    const CHILD: &str = "CCCC_TEST_CLAUDE_RESUME";
    if std::env::var(CHILD).ok().as_deref() == Some(test) {
        return false;
    }
    let temp = tempfile::tempdir().expect("isolated test process");
    // The fixture never invokes MCP. Supply a private public-launcher path so
    // native launch setup does not depend on a user's installed CCCC executable.
    let launcher = temp.path().join("cccc");
    std::fs::write(&launcher, "#!/bin/sh\nexit 0\n").expect("fixture CCCC launcher");
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755))
        .expect("launcher permissions");
    let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args([
            &format!("ops::local_headless::claude_resume_tests::{test}"),
            "--exact",
            "--nocapture",
        ])
        .env(CHILD, test)
        .env("CCCC_LAUNCHER_PATH", &launcher)
        .env("CCCC_HOME", temp.path().join("home"))
        .env("CODEX_HOME", temp.path().join("codex"))
        .env("CLAUDE_CONFIG_DIR", temp.path().join("claude"))
        .output()
        .expect("isolated fixture test");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    true
}

struct Fixture {
    _temp: tempfile::TempDir,
    home: HomeLayout,
    group: GroupDoc,
    actor: Actor,
    config: PathBuf,
    server: Child,
    control: PathBuf,
}

impl Fixture {
    fn new(mode: &str) -> Self {
        use sha2::{Digest, Sha256};
        use std::os::unix::{fs::MetadataExt, fs::PermissionsExt};
        let temp = tempfile::tempdir().expect("fixture");
        let config = temp.path().join("claude-config");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&config).expect("isolated provider config");
        std::fs::create_dir(&workspace).expect("workspace");
        let executable = temp.path().join("claude");
        std::fs::write(&executable, include_str!("../fixtures/claude_resume.py"))
            .expect("fixture CLI");
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
            .expect("executable");
        std::fs::write(config.join("mode"), mode).expect("mode");
        if mode != "missing_history" {
            let history = config
                .join("projects/workspace")
                .join(format!("{SAVED}.jsonl"));
            std::fs::create_dir_all(history.parent().expect("history parent"))
                .expect("history store");
            std::fs::write(history, format!("{}\n", json!({"type":"user","sessionId":SAVED,"message":{"content":"original history"}}))).expect("saved history");
        }
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let mut group = store.create("Claude resume fixture", "").expect("group");
        group = cccc_core::group_scope::attach(
            &store,
            &group.group_id,
            cccc_core::scope::detect(&workspace).expect("fixture scope"),
        )
        .expect("attach workspace");
        let mut actor = Actor::new("claude-1");
        actor.runtime = ActorRuntime::Claude;
        actor.command = vec![executable.to_string_lossy().into_owned()];
        actor.env = BTreeMap::from([
            (
                "CLAUDE_CONFIG_DIR".into(),
                config.to_string_lossy().into_owned(),
            ),
            (
                "HOME".into(),
                temp.path()
                    .join("provider-home")
                    .to_string_lossy()
                    .into_owned(),
            ),
            ("PATH".into(), std::env::var("PATH").unwrap_or_default()),
        ]);
        group.actors.push(actor.clone());
        group.running = true;
        store.save(&group).expect("save group");
        let mut environment = actor.env.clone();
        super::super::codex_mcp::configure_actor_cli(&mut environment);
        environment.insert(
            "CCCC_HOME".into(),
            home.root().to_string_lossy().into_owned(),
        );
        environment.insert("CCCC_GROUP_ID".into(), group.group_id.clone());
        environment.insert("CCCC_ACTOR_ID".into(), actor.id.clone());
        super::super::runtime_session::record_claude_managed_session(
            &home,
            &group.group_id,
            &actor.id,
            &workspace,
            &actor.command,
            &environment,
            SAVED,
            false,
        )
        .expect("saved receipt");
        let canonical = config.canonicalize().expect("canonical config");
        let digest = format!(
            "{:x}",
            Sha256::digest(canonical.to_string_lossy().as_bytes())
        );
        let control = PathBuf::from("/tmp")
            .join(format!(
                "cc-daemon-{}",
                canonical.metadata().expect("metadata").uid()
            ))
            .join(&digest[..8]);
        let server = std::process::Command::new("python3")
            .arg(&executable)
            .arg("server")
            .arg(&config)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("fixture control server");
        let fixture = Self {
            _temp: temp,
            home,
            group,
            actor,
            config,
            server,
            control,
        };
        fixture.wait(|| fixture.config.join("ready").exists());
        fixture
    }

    fn wait(&self, mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !ready() {
            assert!(Instant::now() < deadline, "fixture deadline elapsed");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn receipt(&self) -> Value {
        let path = self
            .home
            .groups_dir()
            .join(&self.group.group_id)
            .join("state/runtime_sessions/claude-1.json");
        cccc_core::fs::read_json(&path).expect("receipt")
    }

    fn lifecycle(&self, op: &str) -> cccc_contracts::DaemonResponse {
        crate::handle_request(
            &self.home,
            &DaemonRequest {
                v: 1,
                op: op.into(),
                args: json!({"group_id":self.group.group_id,"actor_id":self.actor.id,"by":"user"})
                    .as_object()
                    .expect("arguments")
                    .clone(),
            },
        )
    }

    fn launches(&self) -> usize {
        std::fs::read_to_string(self.config.join("launches"))
            .unwrap_or_default()
            .lines()
            .count()
    }

    fn delivery(&self) -> crate::ops::actor_delivery::DeliveryJob {
        let mut event = Event::new("chat.message", &self.group.group_id);
        event.by = "user".into();
        event.data = json!({"to":[self.actor.id],"text":"PENDING_TASK","message_mode":"send"})
            .as_object()
            .expect("data")
            .clone();
        let store = GroupStore::new(self.home.clone()).expect("store");
        ledger::append(
            &store
                .ledger_path(&self.group.group_id)
                .expect("ledger path"),
            &event,
        )
        .expect("pending message");
        crate::ops::actor_delivery::DeliveryJob {
            home: self.home.clone(),
            group: self.group.clone(),
            actor: self.actor.clone(),
            event,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        super::super::actor_delivery::shutdown_actor(&self.group.group_id, &self.actor.id);
        let _ = super::super::actor_runtime::apply(
            &self.home,
            &self.group,
            &self.actor.id,
            "actor.stop",
        );
        let _ = self.server.kill();
        let _ = self.server.wait();
        let _ = std::fs::remove_dir_all(&self.control);
    }
}

#[test]
fn copied_claude_resume_stops_auto_wake_and_explicit_retry_delivers_the_original_message() {
    if isolated(
        "copied_claude_resume_stops_auto_wake_and_explicit_retry_delivers_the_original_message",
    ) {
        return;
    }
    let f = Fixture::new("copied");
    let job = f.delivery();
    let handled = super::super::actor_delivery_worker::process_batch(
        std::slice::from_ref(&job),
        &mut String::new(),
        &AtomicBool::new(false),
    );
    assert!(handled, "blocked startup leaves the automatic retry lane");
    assert_eq!(f.launches(), 1);
    let failed = f.receipt();
    assert_eq!(failed["status"], "resume_failed");
    assert_eq!(failed["provider_session_id"], SAVED);
    assert_eq!(failed["failure_count"], 1);
    assert!(
        failed["last_resume_error"]
            .as_str()
            .expect("diagnostic")
            .contains("different session ID")
    );
    assert!(
        !f.config.join("active").exists(),
        "copied job must be stopped"
    );
    for _ in 0..4 {
        assert!(super::super::actor_delivery_worker::process_batch(
            std::slice::from_ref(&job),
            &mut String::new(),
            &AtomicBool::new(false)
        ));
    }
    assert_eq!(f.launches(), 1, "no repeated provider launch");
    let mut after = f.receipt();
    let mut before = failed.clone();
    for document in [&mut after, &mut before] {
        document
            .as_object_mut()
            .expect("fixture operation")
            .remove("updated_at");
        document
            .as_object_mut()
            .expect("fixture operation")
            .remove("last_recovery_decision");
    }
    assert_eq!(after, before);
    assert!(
        !f.config.join("received").exists(),
        "pending task was not sent"
    );
    assert_eq!(
        super::super::runtime_delivery::latest_state(
            &f.home,
            &f.group.group_id,
            &f.actor.id,
            &job.event.id
        )
        .expect("delivery state")
        .expect("failed delivery")
        .0,
        "failed"
    );

    std::fs::write(f.config.join("mode"), "healthy").expect("correct provider behavior");
    let started = f.lifecycle("actor_start");
    assert!(started.ok, "{:?}", started.error);
    f.wait(|| f.config.join("received").exists());
    assert_eq!(f.launches(), 2);
    assert_eq!(f.receipt()["provider_session_id"], SAVED);
    assert_eq!(f.receipt()["failure_count"], 0);
    assert_eq!(f.receipt()["last_resume_error"], "");
    let messages = std::fs::read_to_string(f.config.join("received")).expect("native input");
    assert_eq!(messages.matches("PENDING_TASK").count(), 1);
    assert!(
        f.config
            .join("projects/workspace")
            .join(format!("{SAVED}.jsonl"))
            .exists()
    );
}

#[test]
fn missing_claude_history_does_not_become_a_fresh_session_on_the_second_start() {
    if isolated("missing_claude_history_does_not_become_a_fresh_session_on_the_second_start") {
        return;
    }
    let f = Fixture::new("missing_history");
    let error = super::super::actor_runtime::apply(&f.home, &f.group, &f.actor.id, "actor.start")
        .expect_err("missing history");
    assert_eq!(
        error.code,
        super::super::actor_runtime::CLAUDE_RESUME_FAILED,
        "{}",
        error.message
    );
    assert_eq!(f.receipt()["status"], "resume_failed");
    assert_eq!(f.receipt()["provider_session_id"], SAVED);
    assert!(
        f.receipt()["last_resume_error"]
            .as_str()
            .expect("diagnostic")
            .contains("durable transcript")
    );
    assert!(
        super::super::actor_runtime::apply(&f.home, &f.group, &f.actor.id, "actor.start").is_err()
    );
    assert_eq!(f.launches(), 1);
    std::fs::write(f.config.join("mode"), "healthy").expect("allow a fresh session");
    let reset = f.lifecycle("actor_new_session");
    assert!(reset.ok, "{:?}", reset.error);
    assert_ne!(f.receipt()["provider_session_id"], SAVED);
}

#[test]
fn claude_trust_prompt_never_receives_tasks_and_stops_after_nontrust_resume_failure() {
    if isolated("claude_trust_prompt_never_receives_tasks_and_stops_after_nontrust_resume_failure")
    {
        return;
    }
    let f = Fixture::new("untrusted");
    let started = f.lifecycle("actor_start");
    assert!(started.ok, "{:?}", started.error);
    f.wait(|| f.config.join("terminal_ready").exists());
    let job = f.delivery();
    assert!(!super::super::actor_delivery_worker::process_batch(
        std::slice::from_ref(&job),
        &mut String::new(),
        &AtomicBool::new(false)
    ));
    assert!(
        !f.config.join("received").exists(),
        "trust PTY is not the managed input path"
    );
    let store = GroupStore::new(f.home.clone()).expect("store");
    let mut pending = f.group.clone();
    pending.actors[0].runtime = ActorRuntime::Antigravity;
    pending.actors[0].normalize_runtime_constraints();
    store
        .save(&pending)
        .expect("save next-launch native settings");
    assert!(!super::super::actor_delivery_worker::process_batch(
        std::slice::from_ref(&job),
        &mut String::new(),
        &AtomicBool::new(false)
    ));
    assert!(
        !f.config.join("received").exists(),
        "saved native settings must not turn the trust PTY into a task surface"
    );
    store.save(&f.group).expect("restore next-launch settings");
    std::fs::write(f.config.join("mode"), "copied").expect("trust accepted, bad resume");
    std::fs::write(f.config.join(".claude.json"), "fixture approval")
        .expect("trust record changed");
    f.wait(|| {
        f.receipt()["status"] == "resume_failed"
            && !cccc_runtime::status(&f.group.group_id, &f.actor.id)
                .is_ok_and(|status| status.running)
    });
    for change in 0..3 {
        std::fs::write(
            f.config.join(".claude.json"),
            format!("later config write {change}"),
        )
        .expect("config rewrite");
    }
    assert_eq!(
        f.launches(),
        2,
        "watcher must stop on the non-trust failure"
    );
    assert!(super::super::actor_delivery_worker::process_batch(
        std::slice::from_ref(&job),
        &mut String::new(),
        &AtomicBool::new(false)
    ));
    assert!(!f.config.join("received").exists());
}

#[test]
fn new_claude_session_retires_pending_trust_recovery_before_removing_the_receipt() {
    if isolated("new_claude_session_retires_pending_trust_recovery_before_removing_the_receipt") {
        return;
    }
    let f = Fixture::new("untrusted");
    let started = f.lifecycle("actor_start");
    assert!(started.ok, "{:?}", started.error);
    f.wait(|| f.config.join("terminal_ready").exists());
    std::fs::write(f.config.join("mode"), "delayed_resume").expect("pending resume");
    std::fs::write(f.config.join(".claude.json"), "fixture trust approval").expect("trust changed");
    f.wait(|| f.config.join("resume_pending").exists());
    let home = f.home.clone();
    let group_id = f.group.group_id.clone();
    let reset = std::thread::spawn(move || {
        crate::handle_request(
            &home,
            &DaemonRequest {
                v: 1,
                op: "actor_new_session".into(),
                args: json!({"group_id":group_id,"actor_id":"claude-1","by":"user"})
                    .as_object()
                    .expect("args")
                    .clone(),
            },
        )
    });
    std::thread::sleep(Duration::from_millis(100));
    // An in-flight old launch can still write its result until cleanup joins it.
    // Its receipt must not be removed before that ownership boundary.
    let kept_until_cleanup = f.receipt()["provider_session_id"] == SAVED;
    std::fs::write(f.config.join("release_resume"), "released").expect("complete pending launch");
    let result = reset.join().expect("reset thread");
    assert!(
        kept_until_cleanup,
        "receipt retirement must follow pending recovery cleanup"
    );
    assert!(result.ok, "{:?}", result.error);
    assert_ne!(f.receipt()["provider_session_id"], SAVED);
    assert_eq!(f.receipt()["status"], "usable");
    assert!(
        f.receipt()["previous_sessions"]
            .as_array()
            .expect("archived sessions")
            .iter()
            .any(|entry| entry["session_id"] == SAVED && entry["reason"] == "new_session"),
        "explicit New Session must archive the saved pointer before retiring it"
    );
    assert!(
        f.config
            .join("projects/workspace")
            .join(format!("{SAVED}.jsonl"))
            .exists(),
        "reset retains provider history"
    );
}

const WRONG: &str = "ca52e69a-6596-4abd-a0ec-3e8690dc70e1";

impl Fixture {
    fn resume_session(&self, session_id: &str) -> cccc_contracts::DaemonResponse {
        crate::handle_request(&self.home, &DaemonRequest {
            v: 1, op: "actor_resume_session".into(),
            args: json!({"group_id":self.group.group_id,"actor_id":self.actor.id,"session_id":session_id,"by":"user"}).as_object().expect("fixture value").clone(),
        })
    }

    fn retarget_history(&self, recorded_cwd: &str) -> PathBuf {
        let cwd = super::super::actor_runtime::working_directory(&self.group, &self.actor)
            .expect("fixture value")
            .canonicalize()
            .expect("fixture value");
        let slug: String = cwd
            .to_string_lossy()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let path = self
            .config
            .join("projects")
            .join(slug)
            .join(format!("{SAVED}.jsonl"));
        std::fs::create_dir_all(path.parent().expect("fixture value")).expect("fixture value");
        std::fs::write(&path, format!("{}\n", json!({"type":"user","cwd":recorded_cwd,"sessionId":SAVED,"message":{"content":"original history"}}))).expect("fixture value");
        path
    }

    fn wrong_receipt(&self) {
        let mut receipt = self.receipt();
        receipt["provider_session_id"] = json!(WRONG);
        receipt["status"] = json!("resume_failed");
        receipt["resume_eligible"] = json!(false);
        receipt["last_resume_error"] = json!("Claude copied the session");
        super::super::runtime_session::restore_snapshot(
            &self.home,
            &self.group.group_id,
            &self.actor.id,
            receipt.as_object(),
        )
        .expect("fixture value");
    }

    fn assert_refused(&self, code: &str) {
        let before = self.receipt();
        let group_before = GroupStore::new(self.home.clone())
            .expect("fixture value")
            .load(&self.group.group_id)
            .expect("fixture value");
        let response = self.resume_session(SAVED);
        assert!(!response.ok, "unexpected success");
        assert_eq!(response.error.expect("fixture value").code, code);
        assert_eq!(self.receipt(), before, "refusal must not modify receipt");
        assert_eq!(
            serde_json::to_value(
                GroupStore::new(self.home.clone())
                    .expect("fixture value")
                    .load(&self.group.group_id)
                    .expect("fixture value")
            )
            .expect("fixture value"),
            serde_json::to_value(group_before).expect("fixture value")
        );
        assert_eq!(self.launches(), 0);
    }
}

#[test]
fn resume_session_valid_retarget_and_archive_idempotence() {
    if isolated("resume_session_valid_retarget_and_archive_idempotence") {
        return;
    }
    let f = Fixture::new("healthy");
    let cwd =
        super::super::actor_runtime::working_directory(&f.group, &f.actor).expect("fixture value");
    f.retarget_history(cwd.to_str().expect("fixture value"));
    f.wrong_receipt();
    let response = f.resume_session(SAVED);
    assert!(response.ok, "{:?}", response.error);
    let receipt = f.receipt();
    assert_eq!(receipt["provider_session_id"], SAVED);
    assert_eq!(receipt["status"], "usable");
    assert_eq!(receipt["previous_sessions"][0]["session_id"], WRONG);
    assert_eq!(
        receipt["previous_sessions"]
            .as_array()
            .expect("fixture value")
            .len(),
        1
    );
    assert!(f.lifecycle("actor_stop").ok);
    assert!(f.resume_session(SAVED).ok);
    assert_eq!(
        f.receipt()["previous_sessions"],
        receipt["previous_sessions"]
    );
    assert!(f.lifecycle("actor_stop").ok);
    assert!(f.lifecycle("actor_start").ok);
    assert_eq!(
        f.receipt()["previous_sessions"],
        receipt["previous_sessions"]
    );
}

#[test]
fn resume_session_missing_transcript_is_untouched() {
    if isolated("resume_session_missing_transcript_is_untouched") {
        return;
    }
    let f = Fixture::new("healthy");
    f.wrong_receipt();
    f.assert_refused("claude_transcript_missing");
}

#[test]
fn resume_session_cwd_mismatch_is_untouched() {
    if isolated("resume_session_cwd_mismatch_is_untouched") {
        return;
    }
    let f = Fixture::new("healthy");
    f.retarget_history("/different/workspace");
    f.wrong_receipt();
    f.assert_refused("claude_workspace_mismatch");
}

#[test]
fn resume_session_live_elsewhere_is_untouched() {
    if isolated("resume_session_live_elsewhere_is_untouched") {
        return;
    }
    let f = Fixture::new("healthy");
    let cwd =
        super::super::actor_runtime::working_directory(&f.group, &f.actor).expect("fixture value");
    f.retarget_history(cwd.to_str().expect("fixture value"));
    f.wrong_receipt();
    cccc_core::fs::write_json(
        &f.config.join("jobs/abcdef12/state.json"),
        &json!({"sessionId":SAVED,"cwd":cwd}),
    )
    .expect("fixture value");
    std::fs::write(f.config.join("active"), "").expect("fixture value");
    f.assert_refused("claude_session_live");
    assert!(
        f.config.join("active").exists(),
        "foreign job was not killed"
    );
}

#[test]
fn resume_session_launch_failure_rolls_back_and_keeps_both_records() {
    if isolated("resume_session_launch_failure_rolls_back_and_keeps_both_records") {
        return;
    }
    let f = Fixture::new("copied");
    let cwd =
        super::super::actor_runtime::working_directory(&f.group, &f.actor).expect("fixture value");
    f.retarget_history(cwd.to_str().expect("fixture value"));
    f.wrong_receipt();
    let before = f.receipt();
    let response = f.resume_session(SAVED);
    assert!(!response.ok);
    assert_eq!(
        response.error.expect("fixture value").code,
        "claude_resume_session_failed"
    );
    let mut after = f.receipt();
    let archive = after
        .as_object_mut()
        .expect("fixture value")
        .remove("previous_sessions")
        .expect("fixture value");
    let mut before = before;
    before
        .as_object_mut()
        .expect("fixture value")
        .remove("previous_sessions");
    assert_eq!(after, before);
    assert!(
        archive
            .as_array()
            .expect("fixture value")
            .iter()
            .any(|r| r["session_id"] == SAVED)
    );
    assert!(
        archive
            .as_array()
            .expect("fixture value")
            .iter()
            .any(|r| r["session_id"] == WRONG)
    );
    assert_eq!(f.launches(), 1, "no fresh fallback");
    assert!(!f.config.join("active").exists());
}

#[test]
fn resume_session_identity_mismatch_is_untouched() {
    if isolated("resume_session_identity_mismatch_is_untouched") {
        return;
    }
    let f = Fixture::new("healthy");
    let cwd =
        super::super::actor_runtime::working_directory(&f.group, &f.actor).expect("fixture value");
    f.retarget_history(cwd.to_str().expect("fixture value"));
    f.wrong_receipt();
    let mut receipt = f.receipt();
    receipt["identity_fingerprint"] = json!("a different account/config identity");
    receipt["identity_without_model_fingerprint"] = json!("a different account/config identity");
    super::super::runtime_session::restore_snapshot(
        &f.home,
        &f.group.group_id,
        &f.actor.id,
        receipt.as_object(),
    )
    .expect("fixture value");
    f.assert_refused("claude_identity_mismatch");
}

#[test]
fn resume_session_foreign_process_reports_pid_without_killing_it() {
    if isolated("resume_session_foreign_process_reports_pid_without_killing_it") {
        return;
    }
    let f = Fixture::new("healthy");
    let cwd =
        super::super::actor_runtime::working_directory(&f.group, &f.actor).expect("fixture value");
    f.retarget_history(cwd.to_str().expect("fixture value"));
    f.wrong_receipt();
    let pid = f.server.id();
    cccc_core::fs::write_json(
        &f.config.join("sessions").join(format!("{pid}.json")),
        &json!({"sessionId":SAVED,"pid":pid,"cwd":cwd,"kind":"interactive"}),
    )
    .expect("fixture value");
    let before = f.receipt();
    let response = f.resume_session(SAVED);
    let error = response.error.expect("fixture value");
    assert_eq!(error.code, "claude_session_live");
    assert!(error.message.contains(&format!("pid {pid}")));
    assert_eq!(f.receipt(), before);
    assert_eq!(f.launches(), 0);
    assert!(super::super::membership_cloudflared::process_is_alive(pid));
}

#[test]
fn resume_session_retains_transcript_model_despite_actor_model_drift() {
    if isolated("resume_session_retains_transcript_model_despite_actor_model_drift") {
        return;
    }
    let f = Fixture::new("healthy");
    let cwd =
        super::super::actor_runtime::working_directory(&f.group, &f.actor).expect("fixture value");
    let history = f.retarget_history(cwd.to_str().expect("fixture value"));
    use std::io::Write;
    writeln!(std::fs::OpenOptions::new().append(true).open(history).expect("fixture value"), "{}",
        json!({"type":"assistant","cwd":cwd,"sessionId":SAVED,"message":{"model":"claude-haiku-4-5","content":[]}})).expect("fixture value");
    f.wrong_receipt();
    let store = GroupStore::new(f.home.clone()).expect("fixture value");
    let mut group = store.load(&f.group.group_id).expect("fixture value");
    group.actors[0]
        .command
        .extend(["--model".into(), "opus".into()]);
    store.save(&group).expect("fixture value");
    let response = f.resume_session(SAVED);
    assert!(response.ok, "{:?}", response.error);
    let receipt = f.receipt();
    assert_eq!(receipt["provider_session_id"], SAVED);
    assert_eq!(receipt["model"], "claude-haiku-4-5");
    assert_eq!(receipt["model_application"]["configured_model"], "opus");
    assert_eq!(
        receipt["model_application"]["configured_model_applied"],
        false
    );
    assert_eq!(
        response.result["model_application"],
        receipt["model_application"]
    );
    let arguments: Vec<String> = serde_json::from_str(
        std::fs::read_to_string(f.config.join("launch_arguments"))
            .expect("fixture value")
            .lines()
            .next()
            .expect("fixture value"),
    )
    .expect("fixture value");
    assert_eq!(arguments, ["--bg", "--resume", SAVED]);
    let received = std::fs::read_to_string(f.config.join("received")).unwrap_or_default();
    assert!(
        !received.contains("/model"),
        "retarget must not apply a model change"
    );
}
