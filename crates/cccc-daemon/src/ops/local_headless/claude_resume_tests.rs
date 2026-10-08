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
    crate::stop_every_runtime(&f.home).expect("flush shutdown");
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
    crate::stop_every_runtime(&f.home).expect("detach");
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
    assert_eq!(
        response.error.expect("refusal").code,
        super::super::actor_runtime::CLAUDE_RESUME_FAILED
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

#[cfg(target_os = "linux")]
#[test]
fn reviewer_scoped_launch_failure_cannot_launch_a_second_job() {
    if isolated("reviewer_scoped_launch_failure_cannot_launch_a_second_job") {
        return;
    }
    if !std::path::Path::new("/run/systemd/system").exists() {
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new("healthy");
    detach_policy(&f);
    super::super::runtime_session::remove(&f.home, &f.group.group_id, &f.actor.id)
        .expect("fresh actor");
    let bin = f._temp.path().join("bin");
    std::fs::create_dir(&bin).expect("private runner dir");
    let runner = bin.join("systemd-run");
    std::fs::write(&runner, "#!/bin/sh\nwhile [ \"$1\" != -- ]; do shift; done\nshift\nif [ \"$1\" = /bin/true ]; then exec /bin/true; fi\n\"$@\"\nexit 1\n").expect("runner starts provider then fails");
    std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o755))
        .expect("runner permissions");
    let store = GroupStore::new(f.home.clone()).expect("store");
    let group = store
        .mutate(&f.group.group_id, |g| {
            g.actors[0].env.insert(
                "PATH".into(),
                format!("{}:{}", bin.display(), f.actor.env["PATH"]),
            );
            Ok(g.clone())
        })
        .expect("private PATH");
    let first = super::super::actor_runtime::apply(&f.home, &group, &f.actor.id, "actor.start");
    assert!(first.is_err());
    assert_eq!(f.launches(), 1);
    assert!(f.config.join("active").exists());
    // This is the same retry performed by the ordinary delivery worker after
    // launch_error maps the failed fresh launch to a retriable io_error.
    let second = super::super::actor_runtime::apply(&f.home, &group, &f.actor.id, "actor.start");
    assert!(second.is_err());
    assert_eq!(
        f.launches(),
        1,
        "failed scoped launch left a job but the next startup launched again"
    );
    let owner =
        super::super::runtime_session::claude_ownership::load(&f.home, &f.group.group_id, &f.actor)
            .expect("owner")
            .expect("durable fence");
    assert!(owner.uncertain);
    assert!(
        !owner.session_id.is_empty(),
        "resolve the exact reported job even after scope failure"
    );
    let job = f.delivery();
    assert!(
        super::super::actor_delivery_worker::process_batch(
            &[job],
            &mut String::new(),
            &AtomicBool::new(false)
        ),
        "automatic delivery leaves its retry loop at the durable fence"
    );
    assert_eq!(f.launches(), 1);
    assert!(
        f.lifecycle("actor_stop").ok,
        "explicit stop reconciles exact uncertain ownership"
    );
    assert!(!f.config.join("active").exists());
    assert!(
        super::super::runtime_session::claude_ownership::load(&f.home, &f.group.group_id, &f.actor)
            .expect("owner")
            .is_none()
    );
}

#[test]
fn restart_survival_detach_leaves_job_and_releases_observers() {
    if isolated("restart_survival_detach_leaves_job_and_releases_observers") {
        return;
    }
    let f = Fixture::new("healthy");
    detach_policy(&f);
    assert!(f.lifecycle("actor_start").ok);
    super::block_on_managed(super::kill_all_requests());
    assert!(
        f.config.join("active").exists(),
        "forced-exit requests must honor detach"
    );
    let saved = f.receipt()["provider_session_id"].clone();
    let observer = Arc::downgrade(
        &super::supervisor::lookup(&(f.group.group_id.clone(), f.actor.id.clone()))
            .expect("registered session"),
    );
    crate::stop_every_runtime(&f.home).expect("daemon shutdown");
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
fn reviewer_queued_before_handoff_remains_retryable() {
    if isolated("reviewer_queued_before_handoff_remains_retryable") {
        return;
    }
    let f = Fixture::new("healthy");
    detach_policy(&f);
    let store = GroupStore::new(f.home.clone()).expect("store");
    store
        .mutate(&f.group.group_id, |g| {
            g.state = cccc_contracts::GroupState::Paused;
            Ok(())
        })
        .expect("pause before worker");
    let job = f.delivery();
    let report = super::super::actor_delivery::dispatch_to(
        &f.home,
        &f.group,
        &job.event,
        &[f.actor.clone()],
        false,
    );
    assert_eq!(report.queued, 1);
    std::thread::sleep(Duration::from_millis(100));
    crate::stop_every_runtime(&f.home).expect("shutdown queued worker");
    assert!(
        !f.config.join("active").exists(),
        "no provider handoff was attempted"
    );
    assert_eq!(
        super::super::runtime_delivery::claim(
            &f.home,
            &f.group,
            &f.actor,
            &job.event.id,
            "pty",
            false
        )
        .expect("reclaim"),
        super::super::runtime_delivery::ClaimResult::Claimed
    );
}

#[test]
fn reviewer_ambiguous_is_visible_and_requires_explicit_retry() {
    if isolated("reviewer_ambiguous_is_visible_and_requires_explicit_retry") {
        return;
    }
    let f = Fixture::new("healthy");
    detach_policy(&f);
    let job = f.delivery();
    super::super::runtime_delivery::append_state(
        &f.home,
        &f.group.group_id,
        &f.actor.id,
        &f.actor.created_at,
        &job.event.id,
        "pty",
        super::super::runtime_delivery::DeliveryOutcome::Ambiguous("reviewer interrupted handoff"),
    )
    .expect("ambiguous");
    let status = super::super::messaging_status::for_events(
        &f.home,
        &f.group.group_id,
        &[job.event.id.clone()],
    )
    .expect("visible status");
    assert_eq!(
        status[&job.event.id]["obligation_status"][&f.actor.id]["delivery_state"],
        "ambiguous"
    );
    assert_eq!(
        super::super::actor_delivery::dispatch_unread(&f.home, &f.group, &f.actor.id),
        0
    );
    assert_eq!(
        super::super::runtime_delivery::claim(
            &f.home,
            &f.group,
            &f.actor,
            &job.event.id,
            "pty",
            true
        )
        .expect("explicit retry"),
        super::super::runtime_delivery::ClaimResult::Claimed
    );
}

#[test]
fn reviewer_default_shutdown_keeps_stock_completion_clearing() {
    if isolated("reviewer_default_shutdown_keeps_stock_completion_clearing") {
        return;
    }
    let f = Fixture::new("healthy");
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
    crate::stop_every_runtime(&f.home).expect("default shutdown");
    assert_eq!(
        super::super::runtime_delivery::latest_state(
            &f.home,
            &f.group.group_id,
            &f.actor.id,
            &job.event.id
        )
        .expect("state")
        .expect("claim")
        .0,
        "claimed"
    );
}

// This also runs unchanged when combined with session continuity. A successful
// continuity decision (including model-only drift) must pass the detach guard.
#[test]
fn detach_model_change_respects_continuity_decision() {
    if isolated("detach_model_change_respects_continuity_decision") {
        return;
    }
    let f = Fixture::new("healthy");
    detach_policy(&f);
    let continuity = f
        .receipt()
        .get("command_without_model_fingerprint")
        .is_some();
    assert!(f.lifecycle("actor_start").ok);
    super::supervisor::shutdown_with_policy(true).expect("detach");
    let file = f.config.join("jobs/abcdef12/state.json");
    let mut state: Value = cccc_core::fs::read_json(&file).expect("state");
    state["resumeSessionId"] = json!(SAVED);
    state["respawnFlags"] = json!(["--model", "sonnet"]);
    cccc_core::fs::write_json(&file, &state).expect("provider already selected model");
    GroupStore::new(f.home.clone())
        .expect("store")
        .mutate(&f.group.group_id, |g| {
            g.actors[0]
                .command
                .extend(["--model".into(), "sonnet".into()]);
            Ok(())
        })
        .expect("model-only config change");
    let response = f.lifecycle("actor_start");
    if continuity {
        assert!(response.ok, "{:?}", response.error);
        assert!(super::running(&f.group.group_id, &f.actor.id));
    } else {
        // Standalone v0.4.42 keeps its existing model identity policy.
        assert!(!response.ok);
        assert_eq!(
            response.error.expect("blocked").code,
            super::super::actor_runtime::CLAUDE_RESUME_FAILED
        );
    }
    assert_eq!(f.receipt()["provider_session_id"], SAVED);
    assert!(f.config.join("active").exists());
    assert_eq!(f.launches(), 1, "no fresh session/job on model drift");
}

impl Fixture {
    fn reconcile(&self, op: &str, fields: Value) -> cccc_contracts::DaemonResponse {
        let mut args = fields.as_object().expect("fields").clone();
        args.insert("group_id".into(), json!(self.group.group_id));
        args.insert("actor_id".into(), json!(self.actor.id));
        args.insert("by".into(), json!("user"));
        if op == "actor_claude_launch_reset" {
            args.insert("acknowledge".into(), json!(true));
        }
        crate::handle_request(
            &self.home,
            &DaemonRequest {
                v: 1,
                op: op.into(),
                args,
            },
        )
    }
}

#[test]
fn rv2_unidentified_failed_launch_has_explicit_recovery() {
    if isolated("rv2_unidentified_failed_launch_has_explicit_recovery") {
        return;
    }
    let f = Fixture::new("healthy");
    detach_policy(&f);
    let executable = &f.actor.command[0];
    let original = std::fs::read(executable).expect("fixture");
    std::fs::write(
        executable,
        r#"#!/usr/bin/env python3
import sys
if '--version' in sys.argv:
    print('2.1.286 (Claude Code)')
else:
    print('launch refused before any job started', file=sys.stderr)
    sys.exit(1)
"#,
    )
    .expect("fixture");
    let first = f.lifecycle("actor_new_session");
    assert!(!first.ok, "fixture must refuse the first fresh launch");
    let owner =
        super::super::runtime_session::claude_ownership::load(&f.home, &f.group.group_id, &f.actor)
            .expect("fixture")
            .expect("fixture");
    assert!(owner.uncertain && owner.session_id.is_empty());
    assert!(!f.config.join("active").exists());
    std::fs::write(executable, original).expect("fixture");
    for action in [
        "actor_start",
        "actor_stop",
        "actor_restart",
        "actor_new_session",
        "actor_remove",
    ] {
        let response = f.lifecycle(action);
        assert!(!response.ok, "uncertainty must still block {action}");
        assert!(
            response
                .error
                .expect("blocked")
                .message
                .contains("cccc actor reconcile-claude")
        );
    }
    let inspect = f.reconcile("actor_claude_launch_inspect", json!({}));
    assert!(inspect.ok, "supported inspection: {:?}", inspect.error);
    let reset = f.reconcile("actor_claude_launch_reset", Value::Object(inspect.result));
    assert!(reset.ok, "supported reset: {:?}", reset.error);
    assert!(
        f.lifecycle("actor_start").ok,
        "reset restores explicit Start"
    );
    assert!(f.config.join("active").exists());
    assert_eq!(f.launches(), 1);
}

#[test]
fn rv2_proven_spawn_failure_does_not_leave_uncertainty() {
    if isolated("rv2_proven_spawn_failure_does_not_leave_uncertainty") {
        return;
    }
    let f = Fixture::new("healthy");
    detach_policy(&f);
    let executable = &f.actor.command[0];
    let original = std::fs::read(executable).expect("fixture");
    std::fs::write(
        executable,
        r#"#!/bin/sh
if [ "$1" = --version ]; then
    printf '2.1.286 (Claude Code)\n'
    /bin/rm -- "$0"
    exit 0
fi
exit 77
"#,
    )
    .expect("fixture");
    // Keep the fixture away from any real user scope. The effective PATH
    // contains no systemd-run; only the disposable Claude executable is removed.
    GroupStore::new(f.home.clone())
        .expect("fixture")
        .mutate(&f.group.group_id, |g| {
            g.actors[0]
                .env
                .insert("PATH".into(), "/nonexistent-claude-test-path".into());
            Ok(())
        })
        .expect("fixture");
    super::super::runtime_session::remove(&f.home, &f.group.group_id, &f.actor.id)
        .expect("fixture");
    let result = f.lifecycle("actor_new_session");
    eprintln!("known pre-exec failure: {:?}", result.error);
    assert!(!result.ok);
    assert!(
        !std::path::Path::new(executable).exists(),
        "version fixture did not delete its executable"
    );
    assert!(!f.config.join("active").exists());
    let owner =
        super::super::runtime_session::claude_ownership::load(&f.home, &f.group.group_id, &f.actor)
            .expect("fixture");
    std::fs::write(executable, original).expect("fixture");
    assert!(
        owner.is_none_or(|o| !o.uncertain),
        "ENOENT from spawn proves no launcher ran, but the actor is still durably fenced"
    );
}

#[test]
fn claude_launch_reset_checks_generation_original_config_attempt_and_ack() {
    if isolated("claude_launch_reset_checks_generation_original_config_attempt_and_ack") {
        return;
    }
    let f = Fixture::new("healthy");
    detach_policy(&f);
    reviewer_seed_survivor(&f, false);
    super::super::runtime_session::claude_ownership::record(
        &f.home,
        &f.group.group_id,
        &f.actor.id,
        &f.config,
        std::path::Path::new(&f.group.scopes[0].url),
        "",
        true,
    )
    .expect("unknown launch");
    // Configuration drift must not redirect inspection to another provider.
    let replacement = f
        .config
        .parent()
        .expect("parent")
        .join("replacement-config");
    std::fs::create_dir(&replacement).expect("replacement config");
    GroupStore::new(f.home.clone())
        .expect("store")
        .mutate(&f.group.group_id, |group| {
            group.actors[0].env.insert(
                "CLAUDE_CONFIG_DIR".into(),
                replacement.to_string_lossy().into_owned(),
            );
            Ok(())
        })
        .expect("drift");
    let inspection = f.reconcile("actor_claude_launch_inspect", json!({}));
    assert!(inspection.ok, "{:?}", inspection.error);
    assert_eq!(
        inspection.result["config_dir"],
        json!(f.config.canonicalize().expect("original config"))
    );
    assert_eq!(inspection.result["jobs"][0]["short"], "abcdef12");
    let fields = Value::Object(inspection.result);
    for key in [
        "actor_generation",
        "actor_created_at",
        "config_dir",
        "attempt_id",
    ] {
        let mut stale = fields.clone();
        stale[key] = json!("different");
        let reset = f.reconcile("actor_claude_launch_reset", stale);
        assert!(!reset.ok, "stale {key} must be rejected");
        assert_eq!(reset.error.expect("stale").code, "stale_claude_launch");
    }
    let mut no_ack = fields.as_object().expect("fields").clone();
    no_ack.insert("by".into(), json!("user"));
    let response = crate::handle_request(
        &f.home,
        &DaemonRequest {
            v: 1,
            op: "actor_claude_launch_reset".into(),
            args: no_ack.clone(),
        },
    );
    assert_eq!(
        response.error.expect("ack required").code,
        "acknowledgment_required"
    );
    no_ack.insert("by".into(), json!(f.actor.id));
    no_ack.insert("acknowledge".into(), json!(true));
    let response = crate::handle_request(
        &f.home,
        &DaemonRequest {
            v: 1,
            op: "actor_claude_launch_reset".into(),
            args: no_ack,
        },
    );
    assert_eq!(
        response.error.expect("operator only").code,
        "permission_denied"
    );
    let reset = f.reconcile("actor_claude_launch_reset", fields);
    assert!(reset.ok, "{:?}", reset.error);
    assert_eq!(reset.result["jobs_stopped"], 0);
    assert!(f.config.join("active").exists(), "listed job is left alone");
    assert_eq!(f.launches(), 1, "reset does not start or replay work");
    assert!(
        super::super::runtime_session::claude_ownership::load(&f.home, &f.group.group_id, &f.actor)
            .expect("owner")
            .is_none()
    );
}

#[test]
fn proven_spawn_failure_restores_prior_owner_and_preserves_changed_generation() {
    if isolated("proven_spawn_failure_restores_prior_owner_and_preserves_changed_generation") {
        return;
    }
    use super::super::runtime_session::claude_ownership as ownership;
    let f = Fixture::new("healthy");
    let owner = ownership::LaunchOwnership {
        home: f.home.clone(),
        group_id: f.group.group_id.clone(),
        actor_id: f.actor.id.clone(),
    };
    let cwd = std::path::Path::new(&f.group.scopes[0].url);
    ownership::record(
        &f.home,
        &f.group.group_id,
        &f.actor.id,
        &f.config,
        cwd,
        SAVED,
        false,
    )
    .expect("prior owner");
    let prior = ownership::load(&f.home, &f.group.group_id, &f.actor).expect("load");
    let attempt = ownership::begin_launch(&owner, &f.config, cwd, SAVED).expect("begin");
    ownership::restore_unexecuted(&owner, attempt).expect("restore");
    assert_eq!(
        ownership::load(&f.home, &f.group.group_id, &f.actor).expect("load"),
        prior
    );
    let attempt = ownership::begin_launch(&owner, &f.config, cwd, SAVED).expect("begin");
    GroupStore::new(f.home.clone())
        .expect("store")
        .mutate(&f.group.group_id, |group| {
            group.actors[0].generation = "replacement".into();
            Ok(())
        })
        .expect("new generation");
    assert!(ownership::restore_unexecuted(&owner, attempt).is_err());
    assert!(
        ownership::load(&f.home, &f.group.group_id, &f.actor)
            .expect("owner")
            .expect("record")
            .uncertain
    );
}

#[test]
fn rv2_second_restart_still_busy_then_group_stop_cancels_adoption() {
    if isolated("rv2_second_restart_still_busy_then_group_stop_cancels_adoption") {
        return;
    }
    let f = Fixture::new("healthy");
    detach_policy(&f);
    reviewer_seed_survivor(&f, true);
    let locks = crate::dispatch_concurrency::DispatchLocks::default();
    super::super::runtime_restore::spawn(f.home.clone(), locks);
    f.wait(|| {
        f.receipt()["last_resume_attempt_id"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
    });
    let first_probe = f.receipt()["last_resume_attempt_id"].clone();
    crate::stop_every_runtime(&f.home).expect("first shutdown while busy");
    std::thread::sleep(Duration::from_millis(3400));
    assert!(f.config.join("active").exists());
    assert!(!super::running(&f.group.group_id, &f.actor.id));
    crate::runtime_start_gate::allow(&f.home).expect("fixture");
    let locks = crate::dispatch_concurrency::DispatchLocks::default();
    super::super::runtime_restore::spawn(f.home.clone(), locks.clone());
    f.wait(|| f.receipt()["last_resume_attempt_id"] != first_probe);
    assert!(f.config.join("active").exists());
    assert_eq!(f.launches(), 1, "second restart must not spawn another job");
    let start = Instant::now();
    locks
        .with_group_write_blocking(&f.group.group_id, || {
            let response = crate::handle_request(
                &f.home,
                &DaemonRequest {
                    v: 1,
                    op: "group_stop".into(),
                    args: json!({"group_id":f.group.group_id,"by":"user"})
                        .as_object()
                        .expect("fixture")
                        .clone(),
                },
            );
            assert!(response.ok, "{:?}", response.error);
            Ok::<(), crate::dispatch::OpError>(())
        })
        .expect("fixture");
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "Group Stop waited for adoption poll"
    );
    assert!(!f.config.join("active").exists());
    std::thread::sleep(Duration::from_millis(4500));
    assert!(!super::running(&f.group.group_id, &f.actor.id));
    assert_eq!(
        f.launches(),
        1,
        "pending adoption resurrected a stopped group"
    );
    crate::runtime_start_gate::prevent(&f.home).expect("fixture");
}

#[test]
fn rv3_reset_cannot_remove_owner_confirmed_by_background_launch() {
    if isolated("rv3_reset_cannot_remove_owner_confirmed_by_background_launch") {
        return;
    }
    let f = Fixture::new("healthy");
    detach_policy(&f);
    super::super::runtime_session::remove(&f.home, &f.group.group_id, &f.actor.id)
        .expect("fixture");
    let executable = &f.actor.command[0];
    let script = std::fs::read_to_string(executable).expect("fixture");
    let marker = "        print(f\"started · {SHORT}\")";
    assert!(script.contains(marker));
    std::fs::write(executable, script.replace(marker, "        print(f\"started · {SHORT}\", flush=True)\n        while not (root / 'release_launch').exists():\n            time.sleep(0.01)")).expect("fixture");
    let home = f.home.clone();
    let group = f.group.clone();
    let actor = f.actor.clone();
    // The actual delivery-worker wake path owns StartGuard, without the
    // daemon group-dispatch lock. Delay its launcher exit after writing the
    // unidentified attempt so reset deterministically overlaps that launch.
    let launch = std::thread::spawn(move || {
        super::super::actor_runtime::apply(&home, &group, &actor.id, "actor.start")
    });
    f.wait(|| f.config.join("active").exists());
    let inspected = f.reconcile("actor_claude_launch_inspect", json!({}));
    assert!(inspected.ok, "{:?}", inspected.error);
    assert_eq!(inspected.result["session_id"], "");
    assert_eq!(inspected.result["jobs"][0]["short"], "abcdef12");
    let mut args = inspected.result;
    args.insert("by".into(), json!("user"));
    args.insert("acknowledge".into(), json!(true));
    let stale_args = args.clone();
    let home = f.home.clone();
    let group_id = f.group.group_id.clone();
    let locks = crate::dispatch_concurrency::DispatchLocks::default();
    let (sent, received) = std::sync::mpsc::channel();
    let reset = std::thread::spawn(move || {
        let response = locks.with_group_write_blocking(&group_id, || {
            crate::handle_request(
                &home,
                &DaemonRequest {
                    v: 1,
                    op: "actor_claude_launch_reset".into(),
                    args,
                },
            )
        });
        sent.send(response.clone()).expect("fixture");
        response
    });
    let before_launch_finished = received.recv_timeout(Duration::from_secs(2));
    // Release and join both threads before assertions, including on regression.
    std::fs::write(f.config.join("release_launch"), "finish background launch").expect("fixture");
    let started = launch.join().expect("fixture");
    let result = reset.join().expect("fixture");
    assert!(started.is_ok(), "{started:?}");
    use super::super::runtime_session::claude_ownership as ownership;
    let confirmed = ownership::load(&f.home, &f.group.group_id, &f.actor)
        .expect("fixture")
        .expect("fixture");
    assert!(!confirmed.uncertain && !confirmed.session_id.is_empty());
    assert!(
        before_launch_finished.is_ok(),
        "reset must not wait for an active launch"
    );
    assert!(
        !result.ok,
        "reset must not delete an in-flight launch's ownership"
    );
    assert_eq!(
        result.error.expect("fixture").code,
        super::super::actor_runtime::RUNTIME_BUSY
    );
    // Once launch finishes, stale acknowledgment must reject confirmed ownership.
    let stale_reset = f.reconcile("actor_claude_launch_reset", Value::Object(stale_args));
    assert!(!stale_reset.ok);
    assert_eq!(
        stale_reset.error.expect("fixture").code,
        "no_uncertain_launch"
    );
    assert_eq!(
        ownership::load(&f.home, &f.group.group_id, &f.actor).expect("fixture"),
        Some(confirmed)
    );
    assert!(f.config.join("active").exists());
}

#[test]
fn rv3_spawn_rollback_preserves_newer_attempt_exactly() {
    if isolated("rv3_spawn_rollback_preserves_newer_attempt_exactly") {
        return;
    }
    use super::super::runtime_session::claude_ownership as ownership;
    let f = Fixture::new("healthy");
    let owner = ownership::LaunchOwnership {
        home: f.home.clone(),
        group_id: f.group.group_id.clone(),
        actor_id: f.actor.id.clone(),
    };
    let cwd = std::path::Path::new(&f.group.scopes[0].url);
    let attempt = ownership::begin_launch(&owner, &f.config, cwd, "").expect("fixture");
    let home = f.home.clone();
    let group_id = f.group.group_id.clone();
    let actor_id = f.actor.id.clone();
    let config = f.config.clone();
    let workspace = cwd.to_owned();
    std::thread::spawn(move || {
        ownership::record(
            &home, &group_id, &actor_id, &config, &workspace, SAVED, false,
        )
    })
    .join()
    .expect("fixture")
    .expect("fixture");
    let newer = ownership::load(&f.home, &f.group.group_id, &f.actor)
        .expect("fixture")
        .expect("fixture");
    assert!(!newer.uncertain);
    assert!(ownership::restore_unexecuted(&owner, attempt).is_err());
    assert_eq!(
        ownership::load(&f.home, &f.group.group_id, &f.actor).expect("fixture"),
        Some(newer)
    );
}

#[test]
fn rv3_group_block_errors_name_reconciliation_and_identified_reset_refuses() {
    if isolated("rv3_group_block_errors_name_reconciliation_and_identified_reset_refuses") {
        return;
    }
    let f = Fixture::new("healthy");
    detach_policy(&f);
    use super::super::runtime_session::claude_ownership as ownership;
    ownership::record(
        &f.home,
        &f.group.group_id,
        &f.actor.id,
        &f.config,
        std::path::Path::new(&f.group.scopes[0].url),
        "",
        true,
    )
    .expect("fixture");
    for (op, extra) in [
        ("group_stop", json!({})),
        ("group_set_state", json!({"state":"stopped"})),
        ("group_reset", json!({"confirm":f.group.group_id})),
    ] {
        let mut args = extra.as_object().expect("fixture").clone();
        args.insert("group_id".into(), json!(f.group.group_id));
        args.insert("by".into(), json!("user"));
        let response = crate::handle_request(
            &f.home,
            &DaemonRequest {
                v: 1,
                op: op.into(),
                args,
            },
        );
        assert!(!response.ok, "{op} must remain blocked");
        assert!(
            response
                .error
                .expect("fixture")
                .message
                .contains("cccc actor reconcile-claude"),
            "{op} must name recovery"
        );
    }
    reviewer_seed_survivor(&f, false);
    ownership::record(
        &f.home,
        &f.group.group_id,
        &f.actor.id,
        &f.config,
        std::path::Path::new(&f.group.scopes[0].url),
        SAVED,
        true,
    )
    .expect("fixture");
    let inspected = f.reconcile("actor_claude_launch_inspect", json!({}));
    assert!(inspected.ok);
    assert_eq!(inspected.result["resettable"], false);
    let reset = f.reconcile("actor_claude_launch_reset", Value::Object(inspected.result));
    assert!(!reset.ok);
    assert_eq!(
        reset.error.expect("fixture").code,
        "identified_claude_launch"
    );
    assert!(f.config.join("active").exists());
}

#[test]
fn rv3_reset_never_waits_on_launch_guard_under_group_lock() {
    if isolated("rv3_reset_never_waits_on_launch_guard_under_group_lock") {
        return;
    }
    let f = Fixture::new("healthy");
    detach_policy(&f);
    use super::super::runtime_session::claude_ownership as ownership;
    ownership::record(
        &f.home,
        &f.group.group_id,
        &f.actor.id,
        &f.config,
        std::path::Path::new(&f.group.scopes[0].url),
        "",
        true,
    )
    .expect("fixture");
    let before = ownership::load(&f.home, &f.group.group_id, &f.actor).expect("fixture");
    let inspected = f.reconcile("actor_claude_launch_inspect", json!({}));
    assert!(inspected.ok);
    let locks = Arc::new(crate::dispatch_concurrency::DispatchLocks::default());
    let (ready, received) = std::sync::mpsc::channel();
    let mut waiting = None;
    let reset = locks.with_group_write_blocking(&f.group.group_id, || {
        let locks = Arc::clone(&locks);
        let key = (f.group.group_id.clone(), f.actor.id.clone());
        waiting = Some(std::thread::spawn(move || {
            let _launch = super::supervisor::StartGuard::acquire(&key).expect("fixture");
            ready.send(()).expect("fixture");
            // Model a launch waiting on the group lock while it owns StartGuard.
            // A bounded wait also lets a regressed blocking reset unwind safely.
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .expect("fixture");
            runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(2), locks.group_write(&key.0))
                    .await
                    .is_ok()
            })
        }));
        received
            .recv_timeout(Duration::from_secs(2))
            .expect("fixture");
        f.reconcile("actor_claude_launch_reset", Value::Object(inspected.result))
    });
    let acquired_group = waiting.expect("fixture").join().expect("fixture");
    assert!(
        acquired_group,
        "reset must release the group lock without waiting on StartGuard"
    );
    assert!(!reset.ok);
    assert_eq!(
        reset.error.expect("fixture").code,
        super::super::actor_runtime::RUNTIME_BUSY
    );
    assert_eq!(
        ownership::load(&f.home, &f.group.group_id, &f.actor).expect("fixture"),
        before
    );
}
