//! Exercise real Actor lifecycle/delivery against a local Agent View fixture, no provider tasks.
use cccc_contracts::{Actor, ActorRuntime, DaemonRequest, Event};
use cccc_core::{GroupDoc, GroupStore, HomeLayout, ledger};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    process::Child,
    sync::{Arc, atomic::AtomicBool},
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
    assert_eq!(f.receipt(), failed);
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
        f.config
            .join("projects/workspace")
            .join(format!("{SAVED}.jsonl"))
            .exists(),
        "reset retains provider history"
    );
}

fn detach_policy(f: &Fixture) {
    std::fs::write(
        f.home.root().join("settings.yaml"),
        "runtime:\n  claude_daemon_exit: detach\n",
    )
    .expect("detach setting");
}

#[test]
fn detach_primitive_releases_observers() {
    if isolated("detach_primitive_releases_observers") {
        return;
    }
    let f = Fixture::new("healthy");
    detach_policy(&f);
    assert!(f.lifecycle("actor_start").ok);
    let saved = f.receipt()["provider_session_id"].clone();
    let observer = Arc::downgrade(
        &super::supervisor::lookup(&(f.group.group_id.clone(), f.actor.id.clone()))
            .expect("registered session"),
    );
    super::supervisor::shutdown_with_policy(true).expect("observer detach");
    assert!(
        f.config.join("active").exists(),
        "detach must not kill the provider job"
    );
    assert!(super::supervisor::registered_running(&f.group.group_id, &f.actor.id).is_none());
    f.wait(|| observer.upgrade().is_none());
    assert!(!cccc_runtime::status(&f.group.group_id, &f.actor.id).is_ok_and(|s| s.running));
    crate::runtime_start_gate::allow(&f.home).expect("restart start gate");
    super::super::runtime_restore::restore_running(&f.home).expect("restore");
    assert!(super::running(&f.group.group_id, &f.actor.id));
    assert_eq!(f.receipt()["provider_session_id"], saved);
    assert_eq!(f.launches(), 1, "re-adoption must not launch another job");
}

#[test]
fn reviewer_default_stop_missing_workspace_is_idempotent() {
    if isolated("reviewer_default_stop_missing_workspace_is_idempotent") {
        return;
    }
    let f = Fixture::new("healthy");
    std::fs::remove_dir(&f.group.scopes[0].url).expect("remove scratch workspace");
    let stopped = f.lifecycle("actor_stop");
    assert!(stopped.ok, "default stop changed: {:?}", stopped.error);
}

#[test]
fn reviewer_default_interrupted_handoff_remains_deferred() {
    if isolated("reviewer_default_interrupted_handoff_remains_deferred") {
        return;
    }
    let f = Fixture::new("healthy");
    assert!(f.lifecycle("actor_start").ok);
    f.wait(|| {
        cccc_runtime::bracketed_paste_enabled(&f.group.group_id, &f.actor.id).unwrap_or(false)
    });
    let job = f.delivery();
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancellation = Arc::clone(&cancelled);
    let task = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(500));
        cancellation.store(true, std::sync::atomic::Ordering::Release);
    });
    let handled =
        super::super::actor_delivery_worker::process_batch(&[job], &mut String::new(), &cancelled);
    task.join().expect("cancel");
    assert!(
        !handled,
        "non-opt-in interrupted submission now becomes terminal ambiguous"
    );
}

#[test]
fn reviewer_shutdown_flushes_accepted_completion_without_redelivery() {
    if isolated("reviewer_shutdown_flushes_accepted_completion_without_redelivery") {
        return;
    }
    let f = Fixture::new("healthy");
    detach_policy(&f);
    let job = f.delivery();
    super::super::runtime_delivery::claim(&f.home, &f.group, &f.actor, &job.event.id, "pty", false)
        .expect("claim");
    super::super::actor_delivery::record_completion(
        super::super::actor_delivery::DeliveryCompletion {
            group_id: f.group.group_id.clone(),
            actor_id: f.actor.id.clone(),
            actor_created_at: f.actor.created_at.clone(),
            event_id: job.event.id.clone(),
            transport: "pty".into(),
        },
    );
    super::super::actor_delivery::shutdown_with_policy(Some(&f.home));
    assert_eq!(
        super::super::runtime_delivery::claim(
            &f.home,
            &f.group,
            &f.actor,
            &job.event.id,
            "pty",
            false
        )
        .expect("no redelivery"),
        super::super::runtime_delivery::ClaimResult::Terminal("accepted".into())
    );
}

#[test]
fn restart_survival_busy_job_is_retried_until_re_adopted() {
    if isolated("restart_survival_busy_job_is_retried_until_re_adopted") {
        return;
    }
    let f = Fixture::new("healthy");
    detach_policy(&f);
    let output = std::process::Command::new(&f.actor.command[0])
        .args(["--bg", "--resume", SAVED])
        .envs(&f.actor.env)
        .current_dir(&f.group.scopes[0].url)
        .output()
        .expect("surviving provider job");
    assert!(output.status.success());
    let path = f.config.join("jobs/abcdef12/state.json");
    let mut state: Value = cccc_core::fs::read_json(&path).expect("state");
    state["tempo"] = json!("active");
    state["inFlight"] = json!({"tasks":1,"queued":0,"kinds":["prompt"]});
    cccc_core::fs::write_json(&path, &state).expect("busy state");
    let locks = crate::dispatch_concurrency::DispatchLocks::default();
    super::super::runtime_restore::spawn(f.home.clone(), locks.clone());
    f.wait(|| f.receipt()["failure_count"].as_u64().unwrap_or(0) > 0);
    locks
        .with_group_write_blocking(&f.group.group_id, || {
            GroupStore::new(f.home.clone())
                .expect("store")
                .mutate(&f.group.group_id, |group| {
                    group.actors[0].title = "renamed while busy".into();
                    group.running = false;
                    Ok(())
                })
                .map_err(crate::dispatch::OpError::io)
        })
        .expect("metadata update while waiting");
    assert!(
        f.config.join("active").exists(),
        "busy job must be untouched"
    );
    state["tempo"] = json!("idle");
    state["inFlight"] = json!({"tasks":0,"queued":0});
    cccc_core::fs::write_json(&path, &state).expect("settled state");
    f.wait(|| {
        super::running(&f.group.group_id, &f.actor.id)
            && GroupStore::new(f.home.clone())
                .expect("store")
                .load(&f.group.group_id)
                .expect("group")
                .running
    });
    let restored = GroupStore::new(f.home.clone())
        .expect("store")
        .load(&f.group.group_id)
        .expect("restored group");
    assert_eq!(restored.actors[0].title, "renamed while busy");
    assert!(restored.running);
    assert_eq!(f.receipt()["provider_session_id"], SAVED);
    assert_eq!(f.launches(), 1);
}

#[test]
fn restart_survival_explicit_stop_still_kills_exact_job() {
    if isolated("restart_survival_explicit_stop_still_kills_exact_job") {
        return;
    }
    let f = Fixture::new("healthy");
    detach_policy(&f);
    assert!(f.lifecycle("actor_start").ok);
    assert!(f.config.join("active").exists());
    assert!(f.lifecycle("actor_stop").ok);
    assert!(
        !f.config.join("active").exists(),
        "explicit Actor Stop must stop the job"
    );
}

#[test]
fn restart_survival_interrupted_handoff_is_quarantined() {
    if isolated("restart_survival_interrupted_handoff_is_quarantined") {
        return;
    }
    let f = Fixture::new("healthy");
    detach_policy(&f);
    assert!(f.lifecycle("actor_start").ok);
    f.wait(|| {
        cccc_runtime::bracketed_paste_enabled(&f.group.group_id, &f.actor.id).unwrap_or(false)
    });
    let job = f.delivery();
    super::super::runtime_delivery::claim(&f.home, &f.group, &f.actor, &job.event.id, "pty", false)
        .expect("durable claim");
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancel = Arc::clone(&cancelled);
    let thread = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(500));
        cancel.store(true, std::sync::atomic::Ordering::Release);
    });
    assert!(super::super::actor_delivery_worker::process_batch(
        std::slice::from_ref(&job),
        &mut String::new(),
        &cancelled
    ));
    thread.join().expect("cancel");
    super::supervisor::shutdown_with_policy(true).expect("detach");
    let latest = super::super::runtime_delivery::latest_state(
        &f.home,
        &f.group.group_id,
        &f.actor.id,
        &job.event.id,
    )
    .expect("state")
    .expect("claim outcome");
    assert_eq!(latest.0, "ambiguous");
    assert_eq!(
        super::super::runtime_delivery::claim(
            &f.home,
            &f.group,
            &f.actor,
            &job.event.id,
            "pty",
            false
        )
        .expect("no automatic replay"),
        super::super::runtime_delivery::ClaimResult::Terminal("ambiguous".into())
    );
    let store = GroupStore::new(f.home.clone()).expect("store");
    assert!(
        ledger::read_all(&store.ledger_path(&f.group.group_id).expect("ledger path"))
            .expect("retained source")
            .iter()
            .any(|e| e.id == job.event.id)
    );
    assert!(f.config.join("active").exists());
    crate::runtime_start_gate::allow(&f.home).expect("restart gate");
}

#[test]
fn restart_survival_explicit_stop_during_busy_restore_stops_job() {
    if isolated("restart_survival_explicit_stop_during_busy_restore_stops_job") {
        return;
    }
    let f = Fixture::new("healthy");
    detach_policy(&f);
    let output = std::process::Command::new(&f.actor.command[0])
        .args(["--bg", "--resume", SAVED])
        .envs(&f.actor.env)
        .current_dir(&f.group.scopes[0].url)
        .output()
        .expect("survivor");
    assert!(output.status.success());
    let path = f.config.join("jobs/abcdef12/state.json");
    let mut state: Value = cccc_core::fs::read_json(&path).expect("state");
    state["tempo"] = json!("active");
    cccc_core::fs::write_json(&path, &state).expect("busy");
    let locks = crate::dispatch_concurrency::DispatchLocks::default();
    super::super::runtime_restore::spawn(f.home.clone(), locks.clone());
    f.wait(|| f.receipt()["failure_count"].as_u64().unwrap_or(0) > 0);
    // Production dispatch owns this same group lock. Simulate that boundary.
    locks
        .with_group_write_blocking(&f.group.group_id, || {
            let stopped = f.lifecycle("actor_stop");
            assert!(stopped.ok, "{:?}", stopped.error);
            Ok::<(), crate::dispatch::OpError>(())
        })
        .expect("explicit stop");
    assert!(!f.config.join("active").exists());
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(
        f.launches(),
        1,
        "retry must not resurrect an explicitly stopped actor"
    );
    assert!(!super::running(&f.group.group_id, &f.actor.id));
}

#[test]
fn restart_survival_mismatched_receipt_never_launches_fresh() {
    if isolated("restart_survival_mismatched_receipt_never_launches_fresh") {
        return;
    }
    let f = Fixture::new("healthy");
    detach_policy(&f);
    GroupStore::new(f.home.clone())
        .expect("store")
        .mutate(&f.group.group_id, |group| {
            group.actors[0]
                .command
                .extend(["--append-system-prompt".into(), "different-prompt".into()]);
            Ok(())
        })
        .expect("changed launch identity");
    let response = f.lifecycle("actor_start");
    assert!(!response.ok);
    assert!(
        response
            .error
            .expect("refusal")
            .message
            .contains("launch identity")
    );
    assert_eq!(f.launches(), 0);
    assert_eq!(f.receipt()["provider_session_id"], SAVED);
}

// Independent reviewer probes. Offline fixtures only.
fn reviewer_seed_survivor(f: &Fixture, busy: bool) -> PathBuf {
    let output = std::process::Command::new(&f.actor.command[0])
        .args(["--bg", "--resume", SAVED])
        .envs(&f.actor.env)
        .current_dir(&f.group.scopes[0].url)
        .output()
        .expect("scratch job");
    assert!(output.status.success());
    let path = f.config.join("jobs/abcdef12/state.json");
    if busy {
        let mut state: Value = cccc_core::fs::read_json(&path).expect("state");
        state["tempo"] = json!("active");
        cccc_core::fs::write_json(&path, &state).expect("busy");
    }
    path
}

#[test]
fn reviewer_group_stop_kills_unadopted_saved_job() {
    if isolated("reviewer_group_stop_kills_unadopted_saved_job") {
        return;
    }
    let f = Fixture::new("healthy");
    detach_policy(&f);
    reviewer_seed_survivor(&f, true);
    let response = crate::handle_request(
        &f.home,
        &DaemonRequest {
            v: 1,
            op: "group_stop".into(),
            args: json!({"group_id":f.group.group_id,"by":"user"})
                .as_object()
                .expect("args")
                .clone(),
        },
    );
    assert!(response.ok, "{:?}", response.error);
    assert!(
        !f.config.join("active").exists(),
        "successful Group Stop left saved busy provider job running"
    );
}

#[test]
fn reviewer_second_restart_preserves_pending_sibling_restore() {
    if isolated("reviewer_second_restart_preserves_pending_sibling_restore") {
        return;
    }
    let f = Fixture::new("healthy");
    detach_policy(&f);
    let path = reviewer_seed_survivor(&f, true);
    let store = GroupStore::new(f.home.clone()).expect("store");
    store
        .mutate(&f.group.group_id, |g| {
            let mut sibling = f.actor.clone();
            sibling.id = "claude-2".into();
            g.actors.push(sibling);
            Ok(())
        })
        .expect("add sibling");
    // Start restore for the first actor without letting the sibling launch.
    store
        .mutate(&f.group.group_id, |g| {
            g.actors[1].enabled = false;
            Ok(())
        })
        .expect("sibling disabled");
    let locks = crate::dispatch_concurrency::DispatchLocks::default();
    super::super::runtime_restore::spawn(f.home.clone(), locks.clone());
    f.wait(|| f.receipt()["failure_count"].as_u64().unwrap_or(0) > 0);
    locks
        .with_group_write_blocking(&f.group.group_id, || {
            let response = crate::handle_request(
                &f.home,
                &DaemonRequest {
                    v: 1,
                    op: "actor_stop".into(),
                    args: json!({"group_id":f.group.group_id,"actor_id":"claude-2","by":"user"})
                        .as_object()
                        .expect("args")
                        .clone(),
                },
            );
            assert!(response.ok, "{:?}", response.error);
            Ok::<(), crate::dispatch::OpError>(())
        })
        .expect("stop sibling");
    let current = store.load(&f.group.group_id).expect("group");
    assert!(!current.running);
    assert!(current.actors[0].enabled);
    assert_ne!(current.state, cccc_contracts::GroupState::Stopped);
    crate::runtime_start_gate::prevent(&f.home).expect("shutdown fence");
    std::thread::sleep(Duration::from_millis(150));
    let mut state: Value = cccc_core::fs::read_json(&path).expect("state");
    state["tempo"] = json!("idle");
    cccc_core::fs::write_json(&path, &state).expect("settled during absence");
    crate::runtime_start_gate::allow(&f.home).expect("second restart");
    super::super::runtime_restore::restore_running(&f.home).expect("second restore");
    assert!(
        super::running(&f.group.group_id, &f.actor.id),
        "second startup silently skipped still-enabled surviving sibling"
    );
}

#[test]
fn reviewer_new_session_after_scope_drift_stops_old_job() {
    if isolated("reviewer_new_session_after_scope_drift_stops_old_job") {
        return;
    }
    let f = Fixture::new("healthy");
    detach_policy(&f);
    reviewer_seed_survivor(&f, true);
    let new_workspace = f._temp.path().join("new-workspace");
    std::fs::create_dir(&new_workspace).expect("new workspace");
    GroupStore::new(f.home.clone())
        .expect("store")
        .mutate(&f.group.group_id, |g| {
            g.scopes[0].url = new_workspace.to_string_lossy().into_owned();
            Ok(())
        })
        .expect("changed workspace");
    let response = f.lifecycle("actor_new_session");
    assert!(
        response.ok,
        "advertised New Session recovery refused: {:?}",
        response.error
    );
}

#[test]
fn reviewer_busy_job_disappears_and_resumes_exactly() {
    if isolated("reviewer_busy_job_disappears_and_resumes_exactly") {
        return;
    }
    let f = Fixture::new("healthy");
    detach_policy(&f);
    reviewer_seed_survivor(&f, true);
    let locks = crate::dispatch_concurrency::DispatchLocks::default();
    super::super::runtime_restore::spawn(f.home.clone(), locks);
    f.wait(|| f.receipt()["failure_count"].as_u64().unwrap_or(0) > 0);
    std::fs::remove_file(f.config.join("active")).expect("job exited");
    f.wait(|| super::running(&f.group.group_id, &f.actor.id));
    assert_eq!(f.receipt()["provider_session_id"], SAVED);
    assert_eq!(f.launches(), 2, "resume once, do not spin on absent job");
}

#[test]
fn reviewer_pending_restart_and_new_session_do_not_resurrect_old_job() {
    if isolated("reviewer_pending_restart_and_new_session_do_not_resurrect_old_job") {
        return;
    }
    for action in ["actor_restart", "actor_new_session"] {
        let f = Fixture::new("healthy");
        detach_policy(&f);
        reviewer_seed_survivor(&f, true);
        let locks = crate::dispatch_concurrency::DispatchLocks::default();
        super::super::runtime_restore::spawn(f.home.clone(), locks.clone());
        f.wait(|| f.receipt()["failure_count"].as_u64().unwrap_or(0) > 0);
        locks
            .with_group_write_blocking(&f.group.group_id, || {
                let response = f.lifecycle(action);
                assert!(response.ok, "{:?}", response.error);
                Ok::<(), crate::dispatch::OpError>(())
            })
            .expect("explicit action");
        std::thread::sleep(Duration::from_millis(800));
        assert_eq!(f.launches(), 2);
        assert!(super::running(&f.group.group_id, &f.actor.id));
        let expected = if action == "actor_restart" {
            SAVED
        } else {
            "ca52e69a-6596-4abd-a0ec-3e8690dc70e1"
        };
        assert_eq!(f.receipt()["provider_session_id"], expected);
    }
}

#[test]
fn reviewer_busy_retry_releases_group_lock_before_provider_polling() {
    if isolated("reviewer_busy_retry_releases_group_lock_before_provider_polling") {
        return;
    }
    let f = Fixture::new("healthy");
    detach_policy(&f);
    reviewer_seed_survivor(&f, true);
    let locks = crate::dispatch_concurrency::DispatchLocks::default();
    super::super::runtime_restore::spawn(f.home.clone(), locks.clone());
    f.wait(|| f.receipt()["failure_count"].as_u64().unwrap_or(0) > 0);
    f.wait(|| {
        f.receipt()["last_resume_attempt_id"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
    });
    let start = Instant::now();
    locks
        .with_group_write_blocking(&f.group.group_id, || Ok::<(), crate::dispatch::OpError>(()))
        .expect("group action");
    let elapsed = start.elapsed();
    crate::runtime_start_gate::prevent(&f.home).expect("stop retry worker");
    assert!(
        elapsed < Duration::from_millis(200),
        "busy retry held group dispatch lock for {elapsed:?}"
    );
}

#[test]
fn reviewer_unmatched_reporting_uses_effective_private_configuration() {
    if isolated("reviewer_unmatched_reporting_uses_effective_private_configuration") {
        return;
    }
    let f = Fixture::new("healthy");
    detach_policy(&f);
    reviewer_seed_survivor(&f, false);
    let decoy = f._temp.path().join("unused-public-config");
    std::fs::create_dir(&decoy).expect("decoy configuration");
    let store = GroupStore::new(f.home.clone()).expect("store");
    store
        .mutate(&f.group.group_id, |g| {
            g.actors[0].env.insert(
                "CLAUDE_CONFIG_DIR".into(),
                decoy.to_string_lossy().into_owned(),
            );
            Ok(())
        })
        .expect("public configuration");
    super::super::actor_secrets::replace(
        &f.home,
        &f.group.group_id,
        &f.actor.id,
        BTreeMap::from([(
            "CLAUDE_CONFIG_DIR".into(),
            f.config.to_string_lossy().into_owned(),
        )]),
    )
    .expect("private launch configuration");
    super::super::runtime_restore::report_unmatched(&f.home, &store).expect("report");
    let requests = std::fs::read_to_string(f.config.join("requests")).unwrap_or_default();
    assert!(
        requests.contains("list"),
        "reporting inspected public/ambient configuration instead of actual launch configuration"
    );
}
#[test]
fn detach_new_session_uses_original_provider_configuration() {
    if isolated("detach_new_session_uses_original_provider_configuration") {
        return;
    }
    let f = Fixture::new("healthy");
    detach_policy(&f);
    assert!(f.lifecycle("actor_start").ok);
    super::supervisor::shutdown_with_policy(true).expect("detach old job");
    let old =
        super::super::runtime_session::claude_ownership::load(&f.home, &f.group.group_id, &f.actor)
            .expect("owner")
            .expect("durable owner");
    assert_eq!(
        old.config_dir,
        f.config.canonicalize().expect("original config")
    );
    let replacement = Fixture::new("healthy");
    let workspace = &replacement.group.scopes[0].url;
    GroupStore::new(f.home.clone())
        .expect("store")
        .mutate(&f.group.group_id, |g| {
            g.scopes[0].url = workspace.clone();
            g.actors[0].env.insert(
                "CLAUDE_CONFIG_DIR".into(),
                replacement.config.to_string_lossy().into_owned(),
            );
            Ok(())
        })
        .expect("edit config and workspace");
    let response = f.lifecycle("actor_new_session");
    assert!(response.ok, "{:?}", response.error);
    assert!(!f.config.join("active").exists());
    assert!(replacement.config.join("active").exists());
    let owner =
        super::super::runtime_session::claude_ownership::load(&f.home, &f.group.group_id, &f.actor)
            .expect("owner")
            .expect("replacement owner");
    assert_eq!(
        owner.config_dir,
        replacement.config.canonicalize().expect("new config")
    );
    assert_eq!(
        owner.workspace,
        PathBuf::from(workspace)
            .canonicalize()
            .expect("new workspace")
    );
    assert_ne!(owner.session_id, SAVED);
    assert_eq!(f.receipt()["provider_session_id"], owner.session_id);
}

#[test]
fn detach_stopped_state_kills_pending_job() {
    if isolated("detach_stopped_state_kills_pending_job") {
        return;
    }
    let f = Fixture::new("healthy");
    detach_policy(&f);
    reviewer_seed_survivor(&f, true);
    let response = crate::handle_request(
        &f.home,
        &DaemonRequest {
            v: 1,
            op: "group_set_state".into(),
            args: json!({"group_id":f.group.group_id,"by":"user","state":"stopped"})
                .as_object()
                .expect("args")
                .clone(),
        },
    );
    assert!(response.ok, "{:?}", response.error);
    assert!(!f.config.join("active").exists());
}

#[test]
fn detach_archived_empty_receipt_never_stops_a_job() {
    if isolated("detach_archived_empty_receipt_never_stops_a_job") {
        return;
    }
    let f = Fixture::new("healthy");
    detach_policy(&f);
    reviewer_seed_survivor(&f, true);
    let file = f
        .home
        .groups_dir()
        .join(&f.group.group_id)
        .join("state/runtime_sessions/claude-1.json");
    let mut receipt = f.receipt();
    receipt["provider_session_id"] = json!("");
    receipt["status"] = json!("new_session");
    cccc_core::fs::write_json(&file, &receipt).expect("archive");
    assert!(f.lifecycle("actor_stop").ok);
    assert!(
        f.config.join("active").exists(),
        "unmatched job stays untouched"
    );
    assert!(
        !std::fs::read_to_string(f.config.join("requests"))
            .unwrap_or_default()
            .contains("kill")
    );
}

#[test]
fn detach_history_without_running_intent_is_not_auto_started() {
    if isolated("detach_history_without_running_intent_is_not_auto_started") {
        return;
    }
    let f = Fixture::new("healthy");
    detach_policy(&f);
    GroupStore::new(f.home.clone())
        .expect("store")
        .mutate(&f.group.group_id, |g| {
            g.running = false;
            Ok(())
        })
        .expect("not running");
    super::super::runtime_restore::restore_running(&f.home).expect("restore");
    assert_eq!(f.launches(), 0);
    assert!(!super::running(&f.group.group_id, &f.actor.id));
}
