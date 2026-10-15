use super::*;
use cccc_contracts::{DaemonRequest, Event};
use cccc_core::{GroupStore, Scope, ledger};
use serde_json::json;
use std::time::{Duration, Instant};

struct Blocked {
    temp: tempfile::TempDir,
    home: HomeLayout,
    group: GroupDoc,
    actor: Actor,
    result: mpsc::Receiver<bool>,
}
impl Blocked {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("temporary directory");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("temporary home");
        let store = GroupStore::new(home.clone()).expect("group store");
        let mut group = store.create("blocked input", "").expect("create group");
        group.state = GroupState::Active;
        group.running = true;
        group.scopes.push(Scope {
            scope_key: "project".into(),
            url: temp.path().to_string_lossy().into_owned(),
            label: "project".into(),
            git_remote: String::new(),
        });
        group.active_scope_key = "project".into();
        let mut actor = Actor::new("peer");
        actor.runtime = ActorRuntime::Custom;
        actor.submit = ActorSubmit::None;
        actor.command = vec!["sh".into(), "-c".into(), "sleep 60".into()];
        group.actors.push(actor.clone());
        store.save(&group).expect("save group");
        cccc_runtime::start(cccc_runtime::LaunchSpec {
            group_id: group.group_id.clone(),
            actor_id: actor.id.clone(),
            runner: cccc_contracts::RunnerKind::Pty,
            command: vec![
                "sh".into(),
                "-c".into(),
                "stty raw -echo; touch ready; exec sleep 60".into(),
            ],
            cwd: temp.path().into(),
            env: Default::default(),
            cols: 80,
            rows: 24,
        })
        .expect("start blocked runtime");
        let deadline = Instant::now() + Duration::from_secs(3);
        while !temp.path().join("ready").exists() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel = cancelled.clone();
        let group_id = group.group_id.clone();
        let submitted_actor = actor.clone();
        let (tx, result) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let ok = submit_terminal_text(
                &group_id,
                &submitted_actor,
                &"x".repeat(1024 * 1024),
                &cancel,
            );
            tx.send(ok).expect("send submission result");
        });
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !thread.is_finished(),
            "fixture must be blocked writing PTY input"
        );
        assert!(result.try_recv().is_err());
        let worker = DeliveryWorker {
            sender: None,
            cancelled,
            thread: Some(thread),
        };
        assert!(
            workers()
                .lock()
                .expect("worker registry lock")
                .insert((group.group_id.clone(), actor.id.clone()), worker)
                .is_none()
        );
        Self {
            temp,
            home,
            group,
            actor,
            result,
        }
    }
    fn request(&self, op: &str) -> DaemonRequest {
        DaemonRequest {
            v: 1,
            op: op.into(),
            args: json!({"group_id":self.group.group_id,"actor_id":self.actor.id,"by":"user"})
                .as_object()
                .expect("request arguments")
                .clone(),
        }
    }
}
impl Drop for Blocked {
    fn drop(&mut self) {
        shutdown_actor(&self.group.group_id, &self.actor.id);
        let _ = cccc_runtime::stop(&self.group.group_id, &self.actor.id);
        let _ = self.temp.path();
    }
}

#[test]
fn healthy_start_of_blocked_pty_releases_same_group_recovery_permits() {
    let fixture = Blocked::new();
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let locks = crate::dispatch_concurrency::DispatchLocks::default();
    let request = fixture.request("actor_start");
    let permit = runtime.block_on(locks.acquire(&request));
    let home = fixture.home.clone();
    let (tx, rx) = mpsc::channel();
    let start = std::thread::spawn(move || {
        let _permit = permit;
        tx.send(crate::dispatch::dispatch(&home, &request))
            .expect("response");
    });
    let completed = rx.recv_timeout(Duration::from_secs(2));
    let mut blocked = Vec::new();
    for op in ["actor_stop", "actor_restart", "actor_remove", "actor_start"] {
        let request = fixture.request(op);
        if runtime
            .block_on(async {
                tokio::time::timeout(Duration::from_millis(200), locks.acquire(&request)).await
            })
            .is_err()
        {
            blocked.push(op);
        }
    }
    // Rescue the fixture even if the bound assertion is going to fail.
    shutdown_actor(&fixture.group.group_id, &fixture.actor.id);
    let bounded = completed.is_ok();
    let response = completed.unwrap_or_else(|_| {
        rx.recv_timeout(Duration::from_secs(2))
            .expect("rescued response")
    });
    start.join().expect("Start thread");
    assert!(
        !fixture
            .result
            .recv_timeout(Duration::from_secs(1))
            .expect("cancelled input")
    );
    assert!(
        bounded && response.ok,
        "healthy Start must return promptly: {response:?}"
    );
    assert!(
        blocked.is_empty(),
        "healthy Start blocked recovery permits: {blocked:?}"
    );
}

#[test]
fn two_healthy_starts_and_destructive_shutdown_do_not_deadlock() {
    for mode in ["actor", "group"] {
        let fixture = Blocked::new();
        // Group requests use the same serialization as the IPC server.
        let runtime = Arc::new(tokio::runtime::Runtime::new().expect("runtime"));
        let locks = Arc::new(crate::dispatch_concurrency::DispatchLocks::default());
        let (tx, rx) = mpsc::channel();
        let mut joins = Vec::new();
        for _ in 0..2 {
            let runtime = Arc::clone(&runtime);
            let locks = Arc::clone(&locks);
            let home = fixture.home.clone();
            let request = fixture.request("actor_start");
            let tx = tx.clone();
            joins.push(std::thread::spawn(move || {
                let _permit = runtime.block_on(locks.acquire(&request));
                tx.send(crate::dispatch::dispatch(&home, &request))
                    .expect("response");
            }));
        }
        let results = [
            rx.recv_timeout(Duration::from_secs(2)),
            rx.recv_timeout(Duration::from_secs(2)),
        ];
        let before = Instant::now();
        if mode == "actor" {
            shutdown_actor(&fixture.group.group_id, &fixture.actor.id);
        } else {
            shutdown_group(&fixture.group.group_id);
        }
        for join in joins {
            join.join().expect("Start thread");
        }
        assert!(before.elapsed() < Duration::from_secs(1));
        assert!(
            results
                .iter()
                .all(|result| result.as_ref().is_ok_and(|response| response.ok))
        );
        assert!(
            !fixture
                .result
                .recv_timeout(Duration::from_secs(1))
                .expect("cancelled input")
        );
        assert!(
            !workers()
                .lock()
                .expect("workers")
                .contains_key(&(fixture.group.group_id.clone(), fixture.actor.id.clone()))
        );
    }
}

#[test]
fn dead_pty_unblocks_submission_after_healthy_start() {
    let fixture = Blocked::new();
    let before = Instant::now();
    let response = crate::dispatch::dispatch(&fixture.home, &fixture.request("actor_start"));
    cccc_runtime::stop(&fixture.group.group_id, &fixture.actor.id).expect("stop PTY");
    shutdown_actor(&fixture.group.group_id, &fixture.actor.id);
    assert!(before.elapsed() < Duration::from_secs(2));
    assert!(response.ok, "{response:?}");
    assert!(
        !fixture
            .result
            .recv_timeout(Duration::from_secs(1))
            .expect("failed input")
    );
}

#[test]
fn healthy_start_preserves_pending_acceptance_claim_then_drains_once() {
    let fixture = Blocked::new();
    let home = &fixture.home;
    let group = &fixture.group;
    let actor = &fixture.actor;
    let store = GroupStore::new(home.clone()).expect("store");
    let mut event = Event::new("chat.message", &group.group_id);
    event.by = "user".into();
    event.data = json!({"to":[actor.id],"text":"pending","message_mode":"send"})
        .as_object()
        .expect("data")
        .clone();
    let path = store.ledger_path(&group.group_id).expect("ledger path");
    ledger::append(&path, &event).expect("source");
    let job = DeliveryJob {
        home: home.clone(),
        group: group.clone(),
        actor: actor.clone(),
        event: event.clone(),
    };
    let claim = (group.group_id.clone(), actor.id.clone(), event.id.clone());
    assert!(in_flight().lock().expect("claims").insert(claim.clone()));
    let saved = path.with_extension("saved");
    std::fs::rename(&path, &saved).expect("save ledger");
    std::fs::create_dir(&path).expect("block persistence");
    complete_job(&job);
    std::fs::remove_dir(&path).expect("unblock persistence");
    std::fs::rename(&saved, &path).expect("restore ledger");
    assert!(crate::dispatch::dispatch(home, &fixture.request("actor_start")).ok);
    assert!(in_flight().lock().expect("claims").contains(&claim));
    assert_eq!(
        completions()
            .lock()
            .expect("completions")
            .iter()
            .filter(|completion| completion.group_id == group.group_id)
            .count(),
        1
    );
    assert!(
        !enqueue(job),
        "accepted-but-unrecorded input must not be replayed"
    );
    drain_group(home, &group.group_id);
    drain_group(home, &group.group_id);
    assert!(!in_flight().lock().expect("claims").contains(&claim));
    let events = ledger::read_all(&path).expect("ledger");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == "runtime.delivery"
                && event.data["state"] == "accepted"
                && event.data["source_event_id"] == claim.2)
            .count(),
        1
    );
}

#[test]
fn grok_resume_failure_releases_worker_claim_without_retrying() {
    use crate::ops::{runtime_delivery, runtime_session};
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;

    const CHILD: &str = "CCCC_TEST_GROK_DELIVERY_RESUME_CANARY";
    const TEST: &str = "ops::actor_delivery::start_continuity_tests::grok_resume_failure_releases_worker_claim_without_retrying";
    const SAVED: &str = "52b41c61-e23c-4b7c-8b60-809c347451b5";
    if std::env::var_os(CHILD).is_none() {
        // Isolate launcher selection and resume policy from parallel tests. The
        // provider fixture never invokes MCP; this launcher is only a binding.
        let temp = tempfile::tempdir().expect("isolated launcher");
        let launcher = temp.path().join("cccc");
        std::fs::write(&launcher, "#!/bin/sh\nexit 0\n").expect("fixture launcher");
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o700))
            .expect("launcher permissions");
        let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args(["--exact", TEST, "--nocapture"])
            .env(CHILD, "1")
            .env("CCCC_LAUNCHER_PATH", &launcher)
            .env("CCCC_RUNTIME_RESUME", "1")
            .env("CCCC_HOME", temp.path().join("home"))
            .env("HOME", temp.path().join("provider"))
            .env("CODEX_HOME", temp.path().join("codex"))
            .output()
            .expect("isolated Grok delivery canary");
        assert!(
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).contains("running 1 test"),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    // Grok's leader uses a Unix socket, so keep even long system TMPDIRs out
    // of this fixture's paths. Every provider/config/trace file stays private.
    let temp = tempfile::Builder::new()
        .prefix("cccc-resume-")
        .tempdir_in("/tmp")
        .expect("short Grok fixture directory");
    let home = HomeLayout::from_path(temp.path().join("cccc")).expect("home");
    home.initialize().expect("initialize home");
    let workspace = temp.path().join("workspace");
    let provider = temp.path().join("provider");
    let traces = temp.path().join("traces");
    for directory in [&workspace, &provider, &traces] {
        std::fs::create_dir(directory).expect("private fixture directory");
    }
    let executable = temp.path().join("grok");
    // This existing fixture implements Grok inspect/MCP setup, its owned
    // leader, and ACP initialize. It deliberately exits on session/load, so
    // the real saved-conversation startup fails before any prompt can run.
    std::fs::write(
        &executable,
        include_str!("voice_secretary/structured_fixture.py"),
    )
    .expect("write Grok ACP fixture");
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))
        .expect("provider permissions");
    let store = GroupStore::new(home.clone()).expect("store");
    let mut group = store.create("Grok resume delivery", "").expect("group");
    group.state = GroupState::Active;
    group.running = true;
    group.scopes.push(Scope {
        scope_key: "project".into(),
        url: workspace.to_string_lossy().into_owned(),
        label: "project".into(),
        git_remote: String::new(),
    });
    group.active_scope_key = "project".into();
    let mut actor = Actor::new("grok-worker");
    actor.runtime = ActorRuntime::Grok;
    actor.command = vec![executable.to_string_lossy().into_owned()];
    actor.env = BTreeMap::from([
        ("HOME".into(), provider.to_string_lossy().into_owned()),
        (
            "GROK_HOME".into(),
            provider.join("grok").to_string_lossy().into_owned(),
        ),
        (
            "CODEX_HOME".into(),
            provider.join("codex").to_string_lossy().into_owned(),
        ),
        (
            "FIXTURE_EVENTS".into(),
            traces.to_string_lossy().into_owned(),
        ),
        ("SECRETARY_FIXTURE_RUNTIME".into(), "grok".into()),
    ]);
    group.actors.push(actor.clone());
    store.save(&group).expect("save actor");
    runtime_session::record_grok_managed_session(
        &home,
        &group.group_id,
        &actor.id,
        &workspace,
        &actor.command,
        &actor.env,
        SAVED,
        true,
    )
    .expect("retain native conversation");
    let receipt_path = store
        .state_dir(&group.group_id)
        .expect("state directory")
        .join("runtime_sessions")
        .join(format!("{}.json", actor.id));
    let mut event = Event::new("chat.message", &group.group_id);
    event.by = "user".into();
    event.data = json!({"to":[actor.id],"text":"PENDING_GROK_TASK","message_mode":"send"})
        .as_object()
        .expect("message data")
        .clone();
    let path = store.ledger_path(&group.group_id).expect("ledger path");
    ledger::append(&path, &event).expect("durable pending Send");
    let claim = (group.group_id.clone(), actor.id.clone(), event.id.clone());
    assert!(in_flight().lock().expect("claims").insert(claim.clone()));
    runtime_delivery::append_state(
        &home,
        &group.group_id,
        &actor.id,
        &actor.created_at,
        &event.id,
        delivery_transport(&home, &group, &actor),
        runtime_delivery::DeliveryOutcome::Claimed,
    )
    .expect("claim pending Send");
    let worker = spawn_worker(&(group.group_id.clone(), actor.id.clone()));
    worker
        .sender
        .as_ref()
        .expect("worker sender")
        .send(DeliveryJob {
            home: home.clone(),
            group: group.clone(),
            actor: actor.clone(),
            event: event.clone(),
        })
        .expect("queue real managed delivery");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // Observe both effects while the worker is live: shutdown itself calls
        // fail_jobs and must never make this assertion pass. A Retry instead
        // leaves the claim held during the ten-second automatic restart wait.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let state =
                runtime_delivery::latest_state(&home, &group.group_id, &actor.id, &event.id)
                    .expect("delivery state");
            let released = !in_flight().lock().expect("claims").contains(&claim);
            if state.as_ref().is_some_and(|state| state.0 == "failed") && released {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "failed native resume must leave the automatic retry lane before cleanup: state={state:?}, claim_released={released}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!crate::ops::local_headless::running(
            &group.group_id,
            &actor.id
        ));
        let receipt: serde_json::Value =
            cccc_core::fs::read_json(&receipt_path).expect("retained resume receipt");
        assert_eq!(receipt["provider_session_id"], SAVED);
        assert_eq!(receipt["status"], "resume_failed");
        assert_eq!(receipt["failure_count"], 1);
        assert_eq!(receipt["resume_eligible"], false);
        assert!(
            runtime_delivery::pending_sources(&home, &group, &actor, 10)
                .expect("pending sources")
                .iter()
                .any(|source| source.id == event.id),
            "a failed resume must retain the original Send for explicit recovery"
        );
        assert!(
            cccc_core::inbox::cursor(&home, &group.group_id, &actor.id)
                .expect("read cursor")
                .is_none()
        );
        let events = ledger::read_all(&path).expect("delivery ledger");
        let states: Vec<_> = events
            .iter()
            .filter(|item| {
                item.kind == "runtime.delivery" && item.data["source_event_id"] == event.id
            })
            .filter_map(|item| item.data["state"].as_str())
            .collect();
        assert_eq!(states, ["claimed", "failed"]);
        let trace = std::fs::read_to_string(traces.join("startup-trace.jsonl"))
            .expect("provider startup trace");
        let frames: Vec<serde_json::Value> = trace
            .lines()
            .map(|line| serde_json::from_str(line).expect("trace frame"))
            .collect();
        assert_eq!(
            frames
                .iter()
                .filter(|frame| frame["stage"] == "acp:session/load")
                .count(),
            1,
            "the saved conversation is attempted once"
        );
        for forbidden in ["acp:session/new", "acp:session/prompt"] {
            assert!(
                frames.iter().all(|frame| frame["stage"] != forbidden),
                "failed resume issued unexpected provider work: {forbidden}"
            );
        }
    }));
    // Rescue even a failing canary only after all observations were captured.
    worker.shutdown();
    let _ = crate::ops::actor_runtime::apply(&home, &group, &actor.id, "actor.stop");
    crate::ops::actor_respawn_backoff::forget(&group.group_id, &actor.id);
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}
