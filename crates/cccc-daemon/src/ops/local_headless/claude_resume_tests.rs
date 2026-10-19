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

// A successful idempotent Start records an observation; every conversation and
// recovery-gate field must otherwise remain unchanged.
fn assert_start_preserved_receipt(receipt: &std::path::Path, retained: &[u8]) {
    let mut before: serde_json::Value = serde_json::from_slice(retained).expect("old receipt JSON");
    let mut after: serde_json::Value =
        serde_json::from_slice(&std::fs::read(receipt).expect("retained receipt"))
            .expect("new receipt JSON");
    let timestamp = after["updated_at"].as_str().expect("diagnostic timestamp");
    assert!(
        !timestamp.is_empty(),
        "diagnostic timestamp must be present"
    );
    assert_eq!(
        after["last_recovery_decision"],
        serde_json::json!({
            "action": "skip", "reason": "already_running", "at": timestamp,
            "session_id": before["provider_session_id"].as_str().unwrap_or("")
        }),
        "Start must report that the existing owner was retained"
    );
    for document in [&mut before, &mut after] {
        let object = document.as_object_mut().expect("receipt object");
        object.remove("updated_at");
        object.remove("last_recovery_decision");
    }
    assert_eq!(
        after, before,
        "Start must preserve all non-diagnostic receipt fields"
    );
}

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
        &mut vec![job.clone()],
        &mut String::new(),
        &AtomicBool::new(false),
    );
    assert_eq!(
        handled,
        crate::ops::actor_delivery_worker::BatchOutcome::Delivered,
        "blocked startup leaves the automatic retry lane"
    );
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
        assert!(matches!(
            super::super::actor_delivery_worker::process_batch(
                &mut vec![job.clone()],
                &mut String::new(),
                &AtomicBool::new(false)
            ),
            crate::ops::actor_delivery_worker::BatchOutcome::Delivered
        ));
    }
    assert_eq!(f.launches(), 1, "no repeated provider launch");
    let mut after = f.receipt();
    let timestamp = after["updated_at"].as_str().expect("diagnostic timestamp");
    chrono::DateTime::parse_from_rfc3339(timestamp).expect("valid diagnostic timestamp");
    let decision = &after["last_recovery_decision"];
    assert_eq!(decision["at"], timestamp);
    assert_eq!(decision["action"], "block");
    assert_eq!(decision["session_id"], SAVED);
    assert!(
        !decision["reason"]
            .as_str()
            .expect("safe recovery reason")
            .is_empty(),
        "the terminal startup decision must explain why the original message is held"
    );
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
    // The fixture creates `received` before writing the line into it; wait for
    // the delivered text rather than the file's existence.
    f.wait(|| {
        std::fs::read_to_string(f.config.join("received"))
            .is_ok_and(|received| received.contains("PENDING_TASK"))
    });
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
    assert!(matches!(
        super::super::actor_delivery_worker::process_batch(
            &mut vec![job.clone()],
            &mut String::new(),
            &AtomicBool::new(false)
        ),
        crate::ops::actor_delivery_worker::BatchOutcome::Retry
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
    assert!(matches!(
        super::super::actor_delivery_worker::process_batch(
            &mut vec![job.clone()],
            &mut String::new(),
            &AtomicBool::new(false)
        ),
        crate::ops::actor_delivery_worker::BatchOutcome::Retry
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
    assert!(matches!(
        super::super::actor_delivery_worker::process_batch(
            &mut vec![job.clone()],
            &mut String::new(),
            &AtomicBool::new(false)
        ),
        crate::ops::actor_delivery_worker::BatchOutcome::Delivered
    ));
    assert!(!f.config.join("received").exists());
}

#[test]
fn explicit_start_retries_only_the_registered_claude_trust_owner() {
    if isolated("explicit_start_retries_only_the_registered_claude_trust_owner") {
        return;
    }
    let f = Fixture::new("untrusted");
    let started = f.lifecycle("actor_start");
    assert!(started.ok, "{:?}", started.error);
    f.wait(|| f.config.join("terminal_ready").exists());
    assert!(
        super::waiting_for_workspace_trust(&f.group.group_id, &f.actor.id)
            .expect("registered trust owner")
    );
    let trust_pid = cccc_runtime::status(&f.group.group_id, &f.actor.id)
        .expect("trust terminal")
        .pid;
    let receipt = f
        .home
        .groups_dir()
        .join(&f.group.group_id)
        .join("state/runtime_sessions/claude-1.json");
    let mut failed = f.receipt();
    failed["status"] = json!("resume_failed");
    failed["resume_eligible"] = json!(false);
    cccc_core::fs::write_json(&receipt, &failed).expect("retained failure gate");
    let retained = std::fs::read(&receipt).expect("retained conversation receipt");
    let history = f
        .config
        .join("projects/workspace")
        .join(format!("{SAVED}.jsonl"));
    let history_bytes = std::fs::read(&history).expect("original history");
    let store = GroupStore::new(f.home.clone()).expect("store");
    let mut pending = f.group.clone();
    pending.actors[0].runtime = ActorRuntime::Kiro;
    pending.actors[0].runtime_mode = cccc_contracts::RuntimeMode::Acp;
    pending.actors[0].normalize_runtime_constraints();
    store.save(&pending).expect("save next-launch ACP settings");

    let preserved = f.lifecycle("actor_start");
    assert!(preserved.ok, "{:?}", preserved.error);
    assert_eq!(
        cccc_runtime::status(&f.group.group_id, &f.actor.id)
            .expect("same pending trust terminal")
            .pid,
        trust_pid,
        "Start with saved ACP settings must not replace the pending trust owner"
    );
    assert_start_preserved_receipt(&receipt, &retained);
    assert_eq!(f.launches(), 1);
    assert!(!f.config.join("received").exists());

    store
        .save(&f.group)
        .expect("restore matching Claude configuration");
    std::fs::write(f.config.join("mode"), "healthy").expect("allow explicit provider retry");
    let retried = f.lifecycle("actor_start");
    assert!(retried.ok, "{:?}", retried.error);
    assert!(super::running(&f.group.group_id, &f.actor.id));
    f.wait(|| {
        !super::waiting_for_workspace_trust(&f.group.group_id, &f.actor.id)
            .expect("trust owner retirement")
    });
    assert!(
        !super::waiting_for_workspace_trust(&f.group.group_id, &f.actor.id)
            .expect("trust owner retired")
    );
    assert_ne!(
        cccc_runtime::status(&f.group.group_id, &f.actor.id)
            .expect("managed native attachment")
            .pid,
        trust_pid
    );
    assert_eq!(f.launches(), 2);
    assert_eq!(f.receipt()["provider_session_id"], SAVED);
    assert_eq!(f.receipt()["status"], "usable");
    assert_eq!(
        std::fs::read(&history).expect("history retained"),
        history_bytes
    );
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

#[test]
fn mail_sent_to_an_idle_managed_actor_is_announced_without_waiting_for_the_unread_tick() {
    if isolated(
        "mail_sent_to_an_idle_managed_actor_is_announced_without_waiting_for_the_unread_tick",
    ) {
        return;
    }
    let f = Fixture::new("healthy");
    let store = GroupStore::new(f.home.clone()).expect("store");
    store
        .mutate(&f.group.group_id, |group| {
            group.state = cccc_contracts::GroupState::Active;
            cccc_core::actors::add(group, Actor::new("lead"))
        })
        .expect("sender");
    let started = f.lifecycle("actor_start");
    assert!(started.ok, "{:?}", started.error);
    f.wait(|| super::ready_for_mail_notice(&f.group.group_id, &f.actor.id));

    // No automation scheduler runs here: only the send itself can announce it.
    let sent = crate::handle_request(
        &f.home,
        &DaemonRequest {
            v: 1,
            op: "send".into(),
            args: json!({"group_id":f.group.group_id,"by":"lead","to":[f.actor.id],
                "text":"please review","message_mode":"mail"})
            .as_object()
            .expect("arguments")
            .clone(),
        },
    );
    assert!(sent.ok, "{:?}", sent.error);
    let mail_id = sent.result["event"]["id"].as_str().expect("Mail id");
    let events =
        ledger::read_all(&store.ledger_path(&f.group.group_id).expect("ledger")).expect("ledger");
    let notice = events
        .iter()
        .find(|event| event.kind == "system.notify" && event.data["kind"] == "mail_notice")
        .expect("Mail notice issued by the send");
    assert_eq!(notice.data["context"]["actor_id"], f.actor.id);
    assert_eq!(notice.data["context"]["source_event_ids"], json!([mail_id]));
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
        let _ = std::fs::remove_file(
            self.config
                .join("projects/workspace")
                .join(format!("{SAVED}.jsonl")),
        );
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
    let f = Fixture::new("missing_history");
    f.wrong_receipt();
    f.assert_refused("claude_transcript_missing");
}

#[test]
fn resume_session_cwd_mismatch_is_untouched() {
    if isolated("resume_session_cwd_mismatch_is_untouched") {
        return;
    }
    let f = Fixture::new("healthy");
    let path = f.retarget_history("/different/workspace");
    let other = f.config.join("projects/unrelated-project");
    std::fs::create_dir_all(&other).expect("other project");
    std::fs::rename(&path, other.join(path.file_name().expect("filename"))).expect("relocate");
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

fn assert_retarget_receipt_write_failure_rolls_back(
    failure: super::super::actors::RetargetWriteFailure,
) {
    let f = Fixture::new("healthy");
    let cwd = super::super::actor_runtime::working_directory(&f.group, &f.actor).expect("cwd");
    f.retarget_history(cwd.to_str().expect("cwd string"));
    let started = f.lifecycle("actor_new_session");
    assert!(started.ok, "{:?}", started.error);
    let mut before = f.receipt();
    assert_eq!(before["provider_session_id"], WRONG);
    assert!(super::running(&f.group.group_id, &f.actor.id));
    let launches = f.launches();
    let store = GroupStore::new(f.home.clone()).expect("store");
    let group_before = store.load(&f.group.group_id).expect("group before");

    super::super::actors::fail_next_retarget_write(&f.home, failure);
    let response = f.resume_session(SAVED);
    assert!(!response.ok, "receipt write failure must fail retargeting");
    let error = response.error.expect("retarget error");
    assert_eq!(error.code, "claude_resume_session_failed");
    assert!(
        error
            .message
            .contains("injected retarget receipt write failure")
    );

    let mut after = f.receipt();
    let archive = after
        .as_object_mut()
        .expect("receipt object")
        .remove("previous_sessions")
        .expect("retained targets");
    before
        .as_object_mut()
        .expect("old receipt object")
        .remove("previous_sessions");
    assert_eq!(after, before, "restore every original receipt field");
    for session_id in [SAVED, WRONG] {
        assert!(
            archive
                .as_array()
                .expect("archive")
                .iter()
                .any(|entry| entry["session_id"] == session_id),
            "both selected targets must remain archived"
        );
    }
    assert_eq!(f.launches(), launches, "no replacement or rollback restart");
    assert!(!super::running(&f.group.group_id, &f.actor.id));
    assert!(!f.config.join("active").exists());
    let group_after = store.load(&f.group.group_id).expect("group after");
    assert_eq!(
        serde_json::to_value(group_after.actors).expect("actors after"),
        serde_json::to_value(group_before.actors).expect("actors before")
    );
    assert_eq!(group_after.running, group_before.running);
    assert_eq!(group_after.state, group_before.state);
}

#[test]
fn resume_session_receipt_write_failure_before_replace_restores_original_target() {
    if isolated("resume_session_receipt_write_failure_before_replace_restores_original_target") {
        return;
    }
    assert_retarget_receipt_write_failure_rolls_back(
        super::super::actors::RetargetWriteFailure::BeforeReplace,
    );
}

#[test]
fn resume_session_receipt_write_failure_after_replace_restores_original_target() {
    if isolated("resume_session_receipt_write_failure_after_replace_restores_original_target") {
        return;
    }
    assert_retarget_receipt_write_failure_rolls_back(
        super::super::actors::RetargetWriteFailure::AfterReplace,
    );
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

#[test]
fn resume_session_enter_worktree_history_is_accepted() {
    if isolated("resume_session_enter_worktree_history_is_accepted") {
        return;
    }
    let f = Fixture::new("healthy");
    let cwd = super::super::actor_runtime::working_directory(&f.group, &f.actor).expect("cwd");
    let path = f.retarget_history("/original/session/directory");
    use std::io::Write;
    writeln!(
        std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .expect("history"),
        "{}",
        json!({"type":"user","cwd":cwd,"sessionId":SAVED,"message":{"content":"worktree history"}})
    )
    .expect("append");
    f.wrong_receipt();
    let response = f.resume_session(SAVED);
    assert!(response.ok, "{:?}", response.error);
}

#[test]
fn resume_session_latest_cwd_can_match_after_project_migration() {
    if isolated("resume_session_latest_cwd_can_match_after_project_migration") {
        return;
    }
    let f = Fixture::new("healthy");
    let cwd = super::super::actor_runtime::working_directory(&f.group, &f.actor).expect("cwd");
    let path = f.retarget_history(cwd.to_str().expect("cwd string"));
    let other = f.config.join("projects/previous-project");
    std::fs::create_dir_all(&other).expect("other");
    std::fs::rename(&path, other.join(path.file_name().expect("filename"))).expect("move");
    f.wrong_receipt();
    let response = f.resume_session(SAVED);
    assert!(response.ok, "{:?}", response.error);
}

#[test]
fn resume_session_native_worktree_can_match_original_cwd() {
    if isolated("resume_session_native_worktree_can_match_original_cwd") {
        return;
    }
    let f = Fixture::new("healthy");
    let cwd = super::super::actor_runtime::working_directory(&f.group, &f.actor).expect("cwd");
    let path = f.retarget_history("/original/workspace");
    let other = f.config.join("projects/original-project");
    std::fs::create_dir_all(&other).expect("other");
    std::fs::rename(&path, other.join(path.file_name().expect("filename"))).expect("move");
    cccc_core::fs::write_json(
        &f.config.join("jobs/12345678/state.json"),
        &json!({"sessionId":SAVED,"cwd":"/original/workspace","worktreePath":cwd}),
    )
    .expect("native state");
    f.wrong_receipt();
    let response = f.resume_session(SAVED);
    assert!(response.ok, "{:?}", response.error);
}

#[test]
fn ordinary_lifecycle_uses_current_workspace_instead_of_saved_receipt() {
    if isolated("ordinary_lifecycle_uses_current_workspace_instead_of_saved_receipt") {
        return;
    }
    for op in ["actor_start", "actor_restart", "group_start"] {
        let f = Fixture::new("healthy");
        let store = GroupStore::new(f.home.clone()).expect("store");
        let requested = f._temp.path().join("requested-workspace");
        std::fs::create_dir(&requested).expect("requested workspace");
        let scope = cccc_core::scope::detect(&requested).expect("new scope");
        let scope_key = scope.scope_key.clone();
        cccc_core::group_scope::attach(&store, &f.group.group_id, scope)
            .expect("attach new workspace");
        store
            .mutate(&f.group.group_id, |group| {
                group.active_scope_key = scope_key.clone();
                group.actors[0].default_scope_key = scope_key.clone();
                Ok(())
            })
            .expect("configure current workspace");

        let response = f.lifecycle(op);
        if !response.ok {
            let error = response.error.expect("loud workspace refusal");
            assert!(
                error.message.to_ascii_lowercase().contains("workspace"),
                "{op} refused for an unrelated reason: {}",
                error.message
            );
            assert_eq!(
                f.launches(),
                0,
                "a refused {op} must not launch in the old cwd"
            );
            continue;
        }
        let state_path = f.config.join("jobs/abcdef12/state.json");
        f.wait(|| state_path.is_file());
        let state: Value = cccc_core::fs::read_json(&state_path).expect("actual provider job");
        let actual = PathBuf::from(state["cwd"].as_str().expect("actual provider cwd"));
        assert_eq!(
            actual.canonicalize().expect("actual cwd"),
            requested.canonicalize().expect("requested cwd"),
            "{op} must launch in the currently configured workspace"
        );
        assert_eq!(
            f.launches(),
            1,
            "{op} must not attempt the old cwd then fall back"
        );
    }
}

#[test]
fn resume_disabled_starts_fresh_even_when_a_saved_receipt_exists() {
    const TEST: &str = "resume_disabled_starts_fresh_even_when_a_saved_receipt_exists";
    if let Ok(root) = std::env::var("CCCC_TEST_RESUME_OFF_HOME") {
        let home = HomeLayout::from_path(root).expect("seeded receipt home");
        let group_id = std::env::var("CCCC_TEST_RESUME_OFF_GROUP").expect("seeded group");
        let group = GroupStore::new(home.clone())
            .expect("store")
            .load(&group_id)
            .expect("group");
        let response = crate::handle_request(
            &home,
            &DaemonRequest {
                v: 1,
                op: "actor_start".into(),
                args: json!({"group_id":group_id,"actor_id":"claude-1","by":"user"})
                    .as_object()
                    .expect("arguments")
                    .clone(),
            },
        );
        // Retire this subprocess's session even when its startup assertion fails.
        super::super::actor_delivery::shutdown_actor(&group_id, "claude-1");
        let _ = super::super::actor_runtime::apply(&home, &group, "claude-1", "actor.stop");
        assert!(
            response.ok,
            "resume-off must start fresh: {:?}",
            response.error
        );
        return;
    }
    if isolated(TEST) {
        return;
    }
    let f = Fixture::new("healthy");
    assert_eq!(f.receipt()["provider_session_id"], SAVED);
    // Seed while resume is enabled, then pass the off setting at subprocess
    // creation. No unsafe process-wide environment mutation is required.
    let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args([
            &format!("ops::local_headless::claude_resume_tests::{TEST}"),
            "--exact",
            "--nocapture",
        ])
        .env("CCCC_TEST_RESUME_OFF_HOME", f.home.root())
        .env("CCCC_TEST_RESUME_OFF_GROUP", &f.group.group_id)
        .env("CCCC_RUNTIME_RESUME", "off")
        .output()
        .expect("resume-off lifecycle subprocess");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(f.launches(), 1);
    assert_eq!(
        std::fs::read_to_string(f.config.join("launches")).expect("launch audit"),
        "fresh\n"
    );
    let state: Value = cccc_core::fs::read_json(&f.config.join("jobs/abcdef12/state.json"))
        .expect("actual fresh job");
    assert_ne!(state["sessionId"], SAVED);
}

// PROPOSED TESTS ONLY; not run. Append to local_headless/claude_resume_tests.rs.
// Uses the existing isolated local Agent View fixture and lifecycle path.
// The fixture's launcher and live job advertise supported CLI 2.1.286, which
// is outside the model-dialog adapter's exact 2.1.293 allowlist.

#[test]
fn model_drift_on_supported_cli_without_a_dialog_adapter_starts_fresh_before_resume() {
    if isolated("model_drift_on_supported_cli_without_a_dialog_adapter_starts_fresh_before_resume")
    {
        return;
    }
    let f = Fixture::new("healthy");
    let history = f
        .config
        .join("projects/workspace")
        .join(format!("{SAVED}.jsonl"));
    let original_history = std::fs::read(&history).expect("original saved history");
    let store = GroupStore::new(f.home.clone()).expect("store");
    let mut group = store.load(&f.group.group_id).expect("group");
    group.actors[0]
        .command
        .extend(["--model".into(), "opus".into()]);
    store.save(&group).expect("model change");

    let response = f.lifecycle("actor_start");
    assert!(
        response.ok,
        "supported CLI must remain usable: {:?}",
        response.error
    );
    assert!(super::running(&f.group.group_id, &f.actor.id));
    assert_eq!(
        f.launches(),
        1,
        "compatibility must be decided before launching the old resume"
    );
    let args: Vec<String> = serde_json::from_str(
        std::fs::read_to_string(f.config.join("launch_arguments"))
            .expect("launch trace")
            .lines()
            .next()
            .expect("one fresh launch"),
    )
    .expect("argument JSON");
    assert!(!args.iter().any(|arg| arg == "--resume"));
    assert!(args.windows(2).any(|pair| pair == ["--model", "opus"]));
    let receipt = f.receipt();
    assert_ne!(receipt["provider_session_id"], SAVED);
    assert_eq!(receipt["status"], "usable");
    assert_eq!(receipt["model"], "opus");
    assert_eq!(receipt["failure_count"], 0);
    assert!(
        receipt["previous_sessions"]
            .as_array()
            .expect("archive")
            .iter()
            .any(|entry| entry["session_id"] == SAVED)
    );
    assert_eq!(
        std::fs::read(&history).expect("saved history retained"),
        original_history
    );
    let input = std::fs::read_to_string(f.config.join("received")).unwrap_or_default();
    assert!(
        !input.contains("/model"),
        "unknown dialog adapters never send model commands"
    );
}

#[test]
fn explicit_retained_model_survives_the_second_stop_start() {
    if isolated("explicit_retained_model_survives_the_second_stop_start") {
        return;
    }
    let f = Fixture::new("healthy");
    let cwd = super::super::actor_runtime::working_directory(&f.group, &f.actor).expect("cwd");
    let history = f.retarget_history(cwd.to_str().expect("cwd string"));
    use std::io::Write;
    writeln!(
        std::fs::OpenOptions::new()
            .append(true)
            .open(history)
            .expect("history"),
        "{}",
        json!({"type":"assistant","cwd":cwd,"sessionId":SAVED,
            "message":{"model":"claude-haiku-4-5","content":[]}})
    )
    .expect("append model");
    f.wrong_receipt();
    let store = GroupStore::new(f.home.clone()).expect("store");
    let mut group = store.load(&f.group.group_id).expect("group");
    group.actors[0]
        .command
        .extend(["--model".into(), "opus".into()]);
    store.save(&group).expect("actor settings");

    let selected = f.resume_session(SAVED);
    assert!(selected.ok, "{:?}", selected.error);
    // This assertion is already RED at 82b: record_managed drops the marker.
    assert_eq!(f.receipt()["explicit_resume_session_id"], SAVED);
    let stopped = f.lifecycle("actor_stop");
    assert!(stopped.ok, "{:?}", stopped.error);
    let restarted = f.lifecycle("actor_start");
    assert!(restarted.ok, "{:?}", restarted.error);
    let receipt = f.receipt();
    assert_eq!(receipt["provider_session_id"], SAVED);
    assert_eq!(receipt["explicit_resume_session_id"], SAVED);
    assert_eq!(receipt["model"], "claude-haiku-4-5");
    assert_eq!(receipt["model_application"]["configured_model"], "opus");
    assert_eq!(
        receipt["model_application"]["configured_model_applied"],
        false
    );
    assert_eq!(f.launches(), 2);
    for line in std::fs::read_to_string(f.config.join("launch_arguments"))
        .expect("launches")
        .lines()
    {
        let args: Vec<String> = serde_json::from_str(line).expect("arguments");
        assert_eq!(args, ["--bg", "--resume", SAVED]);
    }
    assert!(
        !std::fs::read_to_string(f.config.join("received"))
            .unwrap_or_default()
            .contains("/model")
    );
    let fresh = f.lifecycle("actor_new_session");
    assert!(
        fresh.ok,
        "New Session must explicitly clear the retained selection: {:?}",
        fresh.error
    );
    assert_ne!(f.receipt()["provider_session_id"], SAVED);
    assert!(
        f.receipt()
            .get("explicit_resume_session_id")
            .is_none_or(|id| id == "")
    );
    assert_eq!(f.receipt()["model"], "opus");
}

// PROPOSED TESTS ONLY; not run. Append to local_headless/claude_resume_tests.rs
// together with surviving_worker_fixture_red.patch. No production source edits
// are included. The old worker is represented by the fixture's real local
// control server, with separate durable state/active markers for both jobs.

fn assert_surviving_worker_model_policy(
    actual_model: &str,
    expect_same_id: bool,
    selected: bool,
    version: &str,
) {
    let f = Fixture::new("surviving_model");
    std::fs::write(f.config.join("fixture_cli_version"), version).expect("proven adapter version");
    let cwd = super::super::actor_runtime::working_directory(&f.group, &f.actor).expect("cwd");
    let history = f
        .config
        .join("projects/workspace")
        .join(format!("{SAVED}.jsonl"));
    let original_history = std::fs::read(&history).expect("old transcript");
    let old_state_path = f.config.join("jobs/abcdef12/state.json");
    std::fs::create_dir_all(old_state_path.parent().expect("old job directory"))
        .expect("job directory");
    cccc_core::fs::write_json(
        &old_state_path,
        &json!({
            "sessionId":SAVED, "resumeSessionId":SAVED, "daemonShort":"abcdef12", "cwd":cwd,
            "tempo":"idle", "inFlight":{"tasks":0,"queued":0}, "intent":"history-bearing",
            "linkScanOffset":0, "linkScanPath":history, "outcome":null,
            "respawnFlags":["--model",actual_model]
        }),
    )
    .expect("surviving worker state");
    if selected {
        let mut receipt = f.receipt();
        receipt["explicit_resume_session_id"] = json!(SAVED);
        receipt["model"] = json!(actual_model);
        receipt["model_application"] = json!({"action":"retained", "retained_model":actual_model,
            "configured_model":"opus", "configured_model_applied":false});
        super::super::runtime_session::restore_snapshot(
            &f.home,
            &f.group.group_id,
            &f.actor.id,
            receipt.as_object(),
        )
        .expect("completed durable selection, no pending transaction");
    }
    let original_state = std::fs::read(&old_state_path).expect("old state bytes");
    std::fs::write(f.config.join("active"), "").expect("surviving old worker");
    let store = GroupStore::new(f.home.clone()).expect("store");
    let mut group = store.load(&f.group.group_id).expect("group");
    group.actors[0]
        .command
        .extend(["--model".into(), "opus".into()]);
    store.save(&group).expect("current actor model");

    let started = f.lifecycle("actor_start");
    assert!(
        started.ok,
        "supported surviving worker must not fail model_ownership: {:?}",
        started.error
    );
    assert!(super::running(&f.group.group_id, &f.actor.id));
    let receipt = f.receipt();
    assert_eq!(receipt["status"], "usable");
    assert_eq!(receipt["failure_count"], 0);
    assert_eq!(
        receipt["model"],
        if selected { actual_model } else { "opus" }
    );
    if expect_same_id {
        assert_eq!(receipt["provider_session_id"], SAVED);
        assert_eq!(
            f.launches(),
            0,
            "matching persisted model needs only reattach"
        );
    } else {
        assert_ne!(receipt["provider_session_id"], SAVED);
        assert_eq!(
            f.launches(),
            1,
            "unknown exclusive UI ownership chooses one ordinary fresh launch"
        );
        assert!(
            receipt["previous_sessions"]
                .as_array()
                .expect("archive")
                .iter()
                .any(|entry| entry["session_id"] == SAVED)
        );
        let args: Vec<String> = serde_json::from_str(
            std::fs::read_to_string(f.config.join("launch_arguments"))
                .expect("launch args")
                .lines()
                .next()
                .expect("one launch"),
        )
        .expect("argument JSON");
        assert!(!args.iter().any(|arg| arg == "--resume"));
        assert!(args.windows(2).any(|pair| pair == ["--model", "opus"]));
        let operations =
            std::fs::read_to_string(f.config.join("control_operations")).expect("control trace");
        for line in operations.lines() {
            let operation: Value = serde_json::from_str(line).expect("operation JSON");
            assert!(
                !(operation["short"] == "abcdef12"
                    && (operation["op"] == "attach" || operation["op"] == "kill")),
                "a foreign surviving UI must not receive model input or cleanup"
            );
        }
    }
    assert!(
        f.config.join("active").exists(),
        "old surviving worker remains alive"
    );
    assert_eq!(
        std::fs::read(&old_state_path).expect("old worker metadata"),
        original_state
    );
    assert_eq!(
        std::fs::read(&history).expect("old conversation"),
        original_history
    );
    assert!(
        !std::fs::read_to_string(f.config.join("received"))
            .unwrap_or_default()
            .contains("/model")
    );
}

#[test]
fn unowned_surviving_worker_model_drift_starts_fresh_without_touching_the_old_ui() {
    if isolated("unowned_surviving_worker_model_drift_starts_fresh_without_touching_the_old_ui") {
        return;
    }
    assert_surviving_worker_model_policy("sonnet", false, false, "2.1.293");
}

#[test]
fn surviving_worker_with_already_applied_model_reattaches_without_a_new_launch() {
    if isolated("surviving_worker_with_already_applied_model_reattaches_without_a_new_launch") {
        return;
    }
    assert_surviving_worker_model_policy("opus", true, false, "2.1.293");
}

#[test]
fn unsupported_model_adapter_on_resumed_worker_is_cleaned_before_fresh() {
    if isolated("unsupported_model_adapter_on_resumed_worker_is_cleaned_before_fresh") {
        return;
    }
    let f = Fixture::new("model_adapter");
    std::fs::write(f.config.join("fixture_launcher_version"), "2.1.293").expect("launcher");
    std::fs::write(f.config.join("fixture_worker_version"), "2.1.286")
        .expect("older supported worker");
    let history = f
        .config
        .join("projects/workspace")
        .join(format!("{SAVED}.jsonl"));
    let old_history = std::fs::read(&history).expect("saved transcript");
    let store = GroupStore::new(f.home.clone()).expect("store");
    let mut group = store.load(&f.group.group_id).expect("group");
    group.actors[0]
        .command
        .extend(["--model".into(), "opus".into()]);
    store.save(&group).expect("actor model");
    let started = f.lifecycle("actor_start");
    assert!(started.ok, "{:?}", started.error);
    assert!(super::running(&f.group.group_id, &f.actor.id));
    assert_eq!(
        f.launches(),
        2,
        "one old resume followed by one fresh launch"
    );
    let arguments =
        std::fs::read_to_string(f.config.join("launch_arguments")).expect("launch trace");
    let arguments: Vec<Vec<String>> = arguments
        .lines()
        .map(|line| serde_json::from_str(line).expect("arguments"))
        .collect();
    assert_eq!(arguments[0], ["--bg", "--resume", SAVED]);
    assert!(!arguments[1].iter().any(|arg| arg == "--resume"));
    let operations =
        std::fs::read_to_string(f.config.join("control_operations")).expect("control trace");
    let kills = operations
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("operation"))
        .filter(|op| op["op"] == "kill")
        .count();
    assert_eq!(
        kills, 1,
        "the newly owned resume must be stopped before fallback"
    );
    assert_ne!(f.receipt()["provider_session_id"], SAVED);
    assert_eq!(f.receipt()["status"], "usable");
    assert_eq!(f.receipt()["failure_count"], 0);
    assert_eq!(f.receipt()["model"], "opus");
    assert_eq!(
        std::fs::read(&history).expect("retained history"),
        old_history
    );
    assert!(
        !std::fs::read_to_string(f.config.join("received"))
            .unwrap_or_default()
            .contains("/model")
    );
}

#[test]
fn completed_explicit_selection_reattaches_its_surviving_job_without_model_application() {
    if isolated(
        "completed_explicit_selection_reattaches_its_surviving_job_without_model_application",
    ) {
        return;
    }
    assert_surviving_worker_model_policy("sonnet", true, true, "2.1.293");
}

#[test]
fn unsupported_adapter_with_uncertain_owned_cleanup_never_starts_fresh() {
    if isolated("unsupported_adapter_with_uncertain_owned_cleanup_never_starts_fresh") {
        return;
    }
    let f = Fixture::new("model_adapter_cleanup_failure");
    std::fs::write(f.config.join("fixture_launcher_version"), "2.1.293").expect("launcher");
    std::fs::write(f.config.join("fixture_worker_version"), "2.1.286")
        .expect("older supported worker");
    let store = GroupStore::new(f.home.clone()).expect("store");
    let mut group = store.load(&f.group.group_id).expect("group");
    group.actors[0]
        .command
        .extend(["--model".into(), "opus".into()]);
    store.save(&group).expect("actor model");
    let started = f.lifecycle("actor_start");
    assert!(
        !started.ok,
        "uncertain cleanup must refuse any fresh launch"
    );
    assert_eq!(
        started.error.expect("cleanup failure").code,
        super::super::actor_runtime::CLAUDE_RESUME_FAILED
    );
    assert_eq!(f.launches(), 1);
    assert_eq!(f.receipt()["provider_session_id"], SAVED);
    assert_eq!(f.receipt()["status"], "resume_failed");
    assert_eq!(f.receipt()["failure_count"], 1);
    assert!(!super::running(&f.group.group_id, &f.actor.id));
    assert!(
        f.config.join("active").exists(),
        "unknown old cleanup is not an empty slot"
    );
    // Automatic retries use the same failed Opus configuration. The receipt's
    // actual saved model remains old; that must not look like a new model edit.
    let group = store.load(&f.group.group_id).expect("current group");
    for _ in 0..2 {
        let error = super::super::actor_runtime::apply(&f.home, &group, &f.actor.id, "actor.start")
            .expect_err("the same failed launch configuration remains blocked");
        assert_eq!(
            error.code,
            super::super::actor_runtime::CLAUDE_RESUME_FAILED
        );
    }
    assert_eq!(
        f.launches(),
        1,
        "no fresh bypass on automatic retry after failed model drift"
    );
    assert!(
        !std::fs::read_to_string(f.config.join("received"))
            .unwrap_or_default()
            .contains("/model")
    );
}

#[test]
fn supported_older_worker_with_already_applied_model_reattaches_without_an_adapter() {
    if isolated("supported_older_worker_with_already_applied_model_reattaches_without_an_adapter") {
        return;
    }
    assert_surviving_worker_model_policy("opus", true, false, "2.1.261");
}

#[test]
fn resume_off_ignores_completed_selection_but_preserves_a_pending_retarget_guard() {
    const TEST: &str =
        "resume_off_ignores_completed_selection_but_preserves_a_pending_retarget_guard";
    if let Ok(root) = std::env::var("CCCC_TEST_SELECTED_OFF_HOME") {
        let home = HomeLayout::from_path(root).expect("seeded selected home");
        let group_id = std::env::var("CCCC_TEST_SELECTED_OFF_GROUP").expect("group");
        let pending = std::env::var("CCCC_TEST_SELECTED_OFF_PENDING").as_deref() == Ok("1");
        let group = GroupStore::new(home.clone())
            .expect("store")
            .load(&group_id)
            .expect("group");
        let request = |op: &str, args: Value| {
            crate::handle_request(
                &home,
                &DaemonRequest {
                    v: 1,
                    op: op.into(),
                    args: args.as_object().expect("arguments").clone(),
                },
            )
        };
        let refused = request(
            "actor_resume_session",
            json!({"group_id":group_id,"actor_id":"claude-1","session_id":SAVED,"by":"user"}),
        );
        assert!(
            !refused.ok,
            "explicit retarget must still refuse the disabled setting"
        );
        assert_eq!(
            refused.error.expect("disabled refusal").code,
            "claude_resume_disabled"
        );
        let started = request(
            "actor_start",
            json!({"group_id":group_id,"actor_id":"claude-1","by":"user"}),
        );
        super::super::actor_delivery::shutdown_actor(&group_id, "claude-1");
        let _ = super::super::actor_runtime::apply(&home, &group, "claude-1", "actor.stop");
        assert_eq!(started.ok, !pending, "{:?}", started.error);
        return;
    }
    if isolated(TEST) {
        return;
    }
    for pending in [false, true] {
        let f = Fixture::new("healthy");
        let mut receipt = f.receipt();
        receipt["explicit_resume_session_id"] = json!(SAVED);
        receipt["explicit_resume_pending"] = json!(pending);
        super::super::runtime_session::restore_snapshot(
            &f.home,
            &f.group.group_id,
            &f.actor.id,
            receipt.as_object(),
        )
        .expect("durable selected target");
        let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                &format!("ops::local_headless::claude_resume_tests::{TEST}"),
                "--exact",
                "--nocapture",
            ])
            .env("CCCC_TEST_SELECTED_OFF_HOME", f.home.root())
            .env("CCCC_TEST_SELECTED_OFF_GROUP", &f.group.group_id)
            .env(
                "CCCC_TEST_SELECTED_OFF_PENDING",
                if pending { "1" } else { "0" },
            )
            .env("CCCC_RUNTIME_RESUME", "off")
            .output()
            .expect("selected resume-off subprocess");
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(f.launches(), usize::from(!pending));
        if !pending {
            assert_eq!(
                std::fs::read_to_string(f.config.join("launches")).expect("launch trace"),
                "fresh\n"
            );
            let state: Value = cccc_core::fs::read_json(&f.config.join("jobs/abcdef12/state.json"))
                .expect("fresh native job");
            assert_ne!(state["sessionId"], SAVED);
        }
        assert_eq!(
            f.receipt()["provider_session_id"],
            SAVED,
            "resume-off does not replace the saved receipt or selected history"
        );
    }
}

#[test]
fn explicit_changed_model_after_failed_resume_starts_fresh_on_supported_cli() {
    const TEST: &str = "explicit_changed_model_after_failed_resume_starts_fresh_on_supported_cli";
    if isolated(TEST) {
        return;
    }
    for operation in ["actor_start", "actor_restart"] {
        let f = Fixture::new("model_adapter");
        std::fs::write(f.config.join("fixture_cli_version"), "2.1.293")
            .expect("supported native model adapter");
        let store = GroupStore::new(f.home.clone()).expect("store");
        let mut group = store.load(&f.group.group_id).expect("group");
        group.actors[0]
            .command
            .extend(["--model".into(), "sonnet".into()]);
        store.save(&group).expect("original Sonnet configuration");
        let original_command = group.actors[0].command.clone();
        let cwd = super::super::actor_runtime::working_directory(&group, &group.actors[0])
            .expect("current workspace");
        let mut environment = f.actor.env.clone();
        super::super::codex_mcp::configure_actor_cli(&mut environment);
        environment.insert(
            "CCCC_HOME".into(),
            f.home.root().to_string_lossy().into_owned(),
        );
        environment.insert("CCCC_GROUP_ID".into(), f.group.group_id.clone());
        environment.insert("CCCC_ACTOR_ID".into(), f.actor.id.clone());
        super::super::runtime_session::record_claude_managed_session(
            &f.home,
            &f.group.group_id,
            &f.actor.id,
            &cwd,
            &original_command,
            &environment,
            SAVED,
            false,
        )
        .expect("saved Sonnet binding");
        let attempt = super::super::runtime_session::prepare_claude_managed_session(
            &f.home,
            &f.group.group_id,
            &f.actor.id,
            &cwd,
            &original_command,
            &environment,
        )
        .expect("original attempt")
        .expect("saved identity");
        super::super::runtime_session::record_claude_resume_failure(
            &f.home,
            &f.group.group_id,
            &f.actor.id,
            &attempt,
            "fixture Sonnet resume failed",
            true,
        )
        .expect("record the failed Sonnet attempt");
        assert_eq!(f.receipt()["status"], "resume_failed");
        assert_eq!(f.receipt()["model"], "sonnet");
        let history = f
            .config
            .join("projects/workspace")
            .join(format!("{SAVED}.jsonl"));
        let old_history = std::fs::read(&history).expect("old transcript");
        *group.actors[0].command.last_mut().expect("model argument") = "opus".into();
        store.save(&group).expect("new Opus launch configuration");

        let result = f.lifecycle(operation);
        assert!(
            result.ok,
            "{operation} with a newly configured model must start fresh: {:?}",
            result.error
        );
        assert!(super::running(&f.group.group_id, &f.actor.id));
        assert_eq!(
            f.launches(),
            1,
            "{operation} must not resume the old conversation first"
        );
        let arguments: Vec<String> = serde_json::from_str(
            std::fs::read_to_string(f.config.join("launch_arguments"))
                .expect("actual CLI arguments")
                .lines()
                .next()
                .expect("one launch"),
        )
        .expect("argument JSON");
        assert!(
            !arguments.iter().any(|arg| arg == "--resume"),
            "explicit lifecycle retry must preserve changed-model fresh policy"
        );
        assert!(arguments.windows(2).any(|pair| pair == ["--model", "opus"]));
        let receipt = f.receipt();
        assert_ne!(receipt["provider_session_id"], SAVED);
        assert_eq!(receipt["status"], "usable");
        assert_eq!(receipt["model"], "opus");
        assert_eq!(receipt["failure_count"], 0);
        assert!(
            receipt["previous_sessions"]
                .as_array()
                .expect("archive")
                .iter()
                .any(|entry| entry["session_id"] == SAVED && entry["model"] == "sonnet")
        );
        assert_eq!(
            std::fs::read(&history).expect("retained transcript"),
            old_history
        );
        assert!(
            !std::fs::read_to_string(f.config.join("received"))
                .unwrap_or_default()
                .contains("/model")
        );
    }
}
