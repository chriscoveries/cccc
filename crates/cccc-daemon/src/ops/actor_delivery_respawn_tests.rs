//! Exercise the real custom-command delivery entry point with an exiting PTY process.
use super::*;
use cccc_contracts::{DaemonRequest, Event};
use cccc_core::{HomeLayout, Scope};

struct Fixture {
    temp: tempfile::TempDir,
    job: DeliveryJob,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("fixture");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let mut group = store.create("custom respawn", "").expect("group");
        group.state = GroupState::Active;
        group.running = true;
        group.scopes.push(Scope {
            scope_key: "project".into(),
            url: temp.path().to_string_lossy().into_owned(),
            label: "project".into(),
            git_remote: String::new(),
        });
        group.active_scope_key = "project".into();
        let mut actor = Actor::new("crasher");
        actor.runtime = ActorRuntime::Custom;
        actor.command = vec![
            "sh".into(),
            "-c".into(),
            "printf 'launch\\n' >> \"$1\"; exit 1".into(),
            "fixture".into(),
            temp.path().join("launches").to_string_lossy().into_owned(),
        ];
        group.actors.push(actor.clone());
        store.save(&group).expect("save group");
        let mut event = Event::new("chat.message", &group.group_id);
        event.by = "user".into();
        event.data = serde_json::json!({"to":[actor.id],"text":"wake"})
            .as_object()
            .expect("event data")
            .clone();
        Self {
            temp,
            job: DeliveryJob {
                home,
                group,
                actor,
                event,
            },
        }
    }

    fn wait_for_exit(&self, launches: usize) {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while self.launches() < launches
            || cccc_runtime::status(&self.job.group.group_id, &self.job.actor.id)
                .is_ok_and(|status| status.running)
        {
            assert!(std::time::Instant::now() < deadline, "fixture did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn launches(&self) -> usize {
        std::fs::read_to_string(self.temp.path().join("launches"))
            .unwrap_or_default()
            .lines()
            .count()
    }

    fn first_automatic_start(&self) {
        // Delivery may fail before or after the shell exits; the attempted launch counts.
        process_batch(
            std::slice::from_ref(&self.job),
            &mut String::new(),
            &AtomicBool::new(false),
        );
        self.wait_for_exit(1);
        assert_eq!(self.launches(), 1);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        super::super::actor_delivery::shutdown_actor(&self.job.group.group_id, &self.job.actor.id);
        let _ = cccc_runtime::stop(&self.job.group.group_id, &self.job.actor.id);
    }
}

#[test]
fn custom_batch_restart_wait_is_interruptible_and_does_not_launch() {
    let fixture = Fixture::new();
    fixture.first_automatic_start();
    let cancelled = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            process_batch(
                std::slice::from_ref(&fixture.job),
                &mut String::new(),
                &cancelled,
            )
        });
        // A repeated automatic start should still be waiting on the 10 s backoff.
        std::thread::sleep(Duration::from_millis(250));
        let stopped = std::time::Instant::now();
        cancelled.store(true, Ordering::Release);
        let _ = worker.join().expect("join delivery");
        assert!(
            stopped.elapsed() < Duration::from_secs(1),
            "cancellation must interrupt the wait"
        );
    });
    // Allow any incorrectly launched shell to finish writing before checking the count.
    fixture.wait_for_exit(1);
    assert_eq!(
        fixture.launches(),
        1,
        "a cancelled automatic restart must not launch another PTY"
    );
}

#[test]
fn explicit_custom_start_and_restart_bypass_automatic_backoff() {
    for op in ["actor_start", "actor_restart"] {
        let fixture = Fixture::new();
        fixture.first_automatic_start();
        let report = super::super::actor_delivery::dispatch(
            &fixture.job.home,
            &fixture.job.group,
            &fixture.job.event,
        );
        assert_eq!(report.queued, 1);
        std::thread::sleep(Duration::from_millis(250));
        let request = DaemonRequest {
            v: 1,
            op: op.into(),
            args: serde_json::json!({"group_id":fixture.job.group.group_id,"actor_id":fixture.job.actor.id,"by":"user"})
                .as_object().expect("request args").clone(),
        };
        let started = std::time::Instant::now();
        let response = crate::dispatch::dispatch(&fixture.job.home, &request);
        assert!(response.ok, "{op}: {response:?}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{op} must not wait for automatic backoff"
        );
        fixture.wait_for_exit(2);
        assert_eq!(
            fixture.launches(),
            2,
            "{op} must cancel the pending automatic wake before launching"
        );
    }
}

#[test]
fn start_during_live_terminal_submission_does_not_repeat_payload() {
    let mut fixture = Fixture::new();
    let marker = "UNIQUE_TASK_8adf9c";
    fixture
        .job
        .event
        .data
        .insert("text".into(), serde_json::json!(marker));
    fixture
        .job
        .event
        .data
        .insert("message_mode".into(), serde_json::json!("send"));
    let store = GroupStore::new(fixture.job.home.clone()).expect("store");
    cccc_core::ledger::append(
        &store
            .ledger_path(&fixture.job.group.group_id)
            .expect("ledger path"),
        &fixture.job.event,
    )
    .expect("durable message");
    let wire = fixture.temp.path().join("wire");
    let ready = fixture.temp.path().join("ready");
    let healthy = cccc_runtime::start(cccc_runtime::LaunchSpec {
        group_id: fixture.job.group.group_id.clone(),
        actor_id: fixture.job.actor.id.clone(),
        runner: cccc_contracts::RunnerKind::Pty,
        command: vec![
            "sh".into(),
            "-c".into(),
            "stty raw -echo; touch \"$2\"; cat > \"$1\"".into(),
            "fixture".into(),
            wire.to_string_lossy().into_owned(),
            ready.to_string_lossy().into_owned(),
        ],
        cwd: fixture.temp.path().to_path_buf(),
        env: Default::default(),
        cols: 120,
        rows: 40,
    })
    .expect("healthy input capture runtime");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !ready.exists() {
        assert!(std::time::Instant::now() < deadline, "PTY not ready");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        super::super::actor_delivery::dispatch(
            &fixture.job.home,
            &fixture.job.group,
            &fixture.job.event
        )
        .queued,
        1
    );
    while !std::fs::read_to_string(&wire)
        .unwrap_or_default()
        .contains(marker)
    {
        assert!(std::time::Instant::now() < deadline, "payload not written");
        std::thread::sleep(Duration::from_millis(10));
    }
    // The payload was written, but the terminal's 1.5-second submit delay has not ended.
    let before = std::fs::read_to_string(&wire).expect("wire before Start");
    assert_eq!(before.matches(marker).count(), 1);
    let request = DaemonRequest { v: 1, op: "actor_start".into(), args: serde_json::json!({"group_id":fixture.job.group.group_id,"actor_id":fixture.job.actor.id,"by":"user"}).as_object().expect("args").clone() };
    let response = crate::dispatch::dispatch(&fixture.job.home, &request);
    assert!(response.ok, "{response:?}");
    let current = cccc_runtime::status(&fixture.job.group.group_id, &fixture.job.actor.id)
        .expect("healthy status");
    assert!(current.running);
    assert_eq!(current.pid, healthy.pid);
    assert_eq!(current.started_at, healthy.started_at);
    let accepted = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let state = super::super::runtime_delivery::latest_state(
            &fixture.job.home,
            &fixture.job.group.group_id,
            &fixture.job.actor.id,
            &fixture.job.event.id,
        )
        .expect("delivery state");
        if state.is_some_and(|state| state.0 == "accepted") {
            break;
        }
        assert!(
            std::time::Instant::now() < accepted,
            "message not accepted after Start"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let bytes = std::fs::read_to_string(&wire).expect("wire after acceptance");
    assert_eq!(
        bytes.matches(marker).count(),
        1,
        "healthy Start repeated payload bytes in the same live terminal: {bytes:?}"
    );
    assert_eq!(bytes.matches("[CCCC] You are crasher").count(), 1);
    // Healthy Start keeps the same idle worker and its preamble state.
    assert!(crate::dispatch::dispatch(&fixture.job.home, &request).ok);
    let mut next = Event::new("chat.message", &fixture.job.group.group_id);
    next.by = "user".into();
    next.data = fixture.job.event.data.clone();
    next.data
        .insert("text".into(), serde_json::json!("UNIQUE_FOLLOWUP_291bfd"));
    cccc_core::ledger::append(
        &store
            .ledger_path(&fixture.job.group.group_id)
            .expect("ledger path"),
        &next,
    )
    .expect("durable followup");
    assert_eq!(
        super::super::actor_delivery::dispatch(&fixture.job.home, &fixture.job.group, &next).queued,
        1
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let state = super::super::runtime_delivery::latest_state(
            &fixture.job.home,
            &fixture.job.group.group_id,
            &fixture.job.actor.id,
            &next.id,
        )
        .expect("followup state");
        let bytes = std::fs::read_to_string(&wire).unwrap_or_default();
        if state.is_some_and(|state| state.0 == "accepted")
            && bytes.contains("UNIQUE_FOLLOWUP_291bfd")
        {
            assert_eq!(bytes.matches(marker).count(), 1);
            assert_eq!(bytes.matches("UNIQUE_FOLLOWUP_291bfd").count(), 1);
            assert_eq!(
                bytes.matches("[CCCC] You are crasher").count(),
                1,
                "healthy Start must preserve the session preamble"
            );
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "followup not accepted"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn blocked_delivery_does_not_hold_healthy_start_or_recovery_permits() {
    let mut fixture = Fixture::new();
    fixture.job.actor.submit = cccc_contracts::ActorSubmit::None;
    fixture.job.group.actors[0] = fixture.job.actor.clone();
    let store = GroupStore::new(fixture.job.home.clone()).expect("group store");
    store.save(&fixture.job.group).expect("save group");
    fixture
        .job
        .event
        .data
        .insert("text".into(), serde_json::json!("z".repeat(1024 * 1024)));
    fixture
        .job
        .event
        .data
        .insert("message_mode".into(), serde_json::json!("send"));
    cccc_core::ledger::append(
        &store
            .ledger_path(&fixture.job.group.group_id)
            .expect("ledger path"),
        &fixture.job.event,
    )
    .expect("persist input event");
    let before = cccc_runtime::start(cccc_runtime::LaunchSpec {
        group_id: fixture.job.group.group_id.clone(),
        actor_id: fixture.job.actor.id.clone(),
        runner: cccc_contracts::RunnerKind::Pty,
        command: vec![
            "sh".into(),
            "-c".into(),
            "stty raw -echo; touch ready; exec sleep 60".into(),
        ],
        cwd: fixture.temp.path().into(),
        env: Default::default(),
        cols: 80,
        rows: 24,
    })
    .expect("start blocked runtime");
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while !fixture.temp.path().join("ready").exists() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        super::super::actor_delivery::dispatch(
            &fixture.job.home,
            &fixture.job.group,
            &fixture.job.event
        )
        .queued,
        1
    );
    std::thread::sleep(Duration::from_millis(1200)); // preamble delay is 500ms, body then fills the PTY buffer
    assert_eq!(
        super::super::runtime_delivery::latest_state(
            &fixture.job.home,
            &fixture.job.group.group_id,
            &fixture.job.actor.id,
            &fixture.job.event.id
        )
        .expect("read delivery state")
        .expect("claimed delivery state")
        .0,
        "claimed"
    );
    let runtime = tokio::runtime::Runtime::new().expect("dispatch runtime");
    let locks = crate::dispatch_concurrency::DispatchLocks::default();
    let request=DaemonRequest {v:1,op:"actor_start".into(),args:serde_json::json!({"group_id":fixture.job.group.group_id,"actor_id":fixture.job.actor.id,"by":"user"}).as_object().expect("Start arguments").clone()};
    let permit = runtime.block_on(locks.acquire(&request));
    let home = fixture.job.home.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let start = std::thread::spawn(move || {
        let _permit = permit;
        tx.send(crate::dispatch::dispatch(&home, &request))
            .expect("send Start response");
    });
    let completed = rx.recv_timeout(Duration::from_secs(2));
    let bounded = completed.is_ok();
    let stop=DaemonRequest {v:1,op:"actor_stop".into(),args:serde_json::json!({"group_id":fixture.job.group.group_id,"actor_id":fixture.job.actor.id}).as_object().expect("Stop arguments").clone()};
    let stop_permit = runtime.block_on(async {
        tokio::time::timeout(Duration::from_millis(250), locks.acquire(&stop)).await
    });
    let stop_blocked = stop_permit.is_err();
    let rescue = std::time::Instant::now();
    super::super::actor_delivery::shutdown_actor(
        &fixture.job.group.group_id,
        &fixture.job.actor.id,
    );
    let response = match completed {
        Ok(response) => response,
        Err(_) => rx
            .recv_timeout(Duration::from_secs(2))
            .expect("rescued Start response"),
    };
    start.join().expect("join Start thread");
    let after = cccc_runtime::status(&fixture.job.group.group_id, &fixture.job.actor.id)
        .expect("runtime status");
    assert_eq!(before.pid, after.pid);
    eprintln!(
        "blocked input: bounded={bounded}, Stop permit blocked={stop_blocked}, direct rescue={:?}, response.ok={}",
        rescue.elapsed(),
        response.ok
    );
    assert!(
        bounded && !stop_blocked,
        "actual delivery can indefinitely wedge healthy Start and prevent Stop"
    );
}

#[test]
fn repeated_healthy_start_without_worker_preserves_runtime_identity() {
    let fixture = Fixture::new();
    let healthy = cccc_runtime::start(cccc_runtime::LaunchSpec {
        group_id: fixture.job.group.group_id.clone(),
        actor_id: fixture.job.actor.id.clone(),
        runner: cccc_contracts::RunnerKind::Pty,
        command: vec!["sh".into(), "-c".into(), "sleep 30".into()],
        cwd: fixture.temp.path().to_path_buf(),
        env: Default::default(),
        cols: 120,
        rows: 40,
    })
    .expect("healthy runtime");
    assert!(healthy.pid.is_some());
    for _ in 0..3 {
        let request = DaemonRequest { v: 1, op: "actor_start".into(), args: serde_json::json!({"group_id":fixture.job.group.group_id,"actor_id":fixture.job.actor.id,"by":"user"}).as_object().expect("args").clone() };
        let started = std::time::Instant::now();
        let response = crate::dispatch::dispatch(&fixture.job.home, &request);
        assert!(response.ok, "{response:?}");
        assert!(started.elapsed() < Duration::from_secs(2));
        let current = cccc_runtime::status(&fixture.job.group.group_id, &fixture.job.actor.id)
            .expect("status");
        assert!(current.running);
        assert_eq!(current.pid, healthy.pid);
        assert_eq!(current.started_at, healthy.started_at);
        assert_eq!(
            fixture.launches(),
            0,
            "configured crashing command was relaunched"
        );
    }
}

#[test]
fn queued_followup_survives_healthy_start_without_repeating_preamble() {
    let mut fixture = Fixture::new();
    let marker = "UNIQUE_TASK_8adf9c";
    fixture
        .job
        .event
        .data
        .insert("text".into(), serde_json::json!(marker));
    fixture
        .job
        .event
        .data
        .insert("message_mode".into(), serde_json::json!("send"));
    let store = GroupStore::new(fixture.job.home.clone()).expect("store");
    cccc_core::ledger::append(
        &store
            .ledger_path(&fixture.job.group.group_id)
            .expect("ledger path"),
        &fixture.job.event,
    )
    .expect("durable message");
    let wire = fixture.temp.path().join("wire");
    let ready = fixture.temp.path().join("ready");
    let healthy = cccc_runtime::start(cccc_runtime::LaunchSpec {
        group_id: fixture.job.group.group_id.clone(),
        actor_id: fixture.job.actor.id.clone(),
        runner: cccc_contracts::RunnerKind::Pty,
        command: vec![
            "sh".into(),
            "-c".into(),
            "stty raw -echo; touch \"$2\"; cat > \"$1\"".into(),
            "fixture".into(),
            wire.to_string_lossy().into_owned(),
            ready.to_string_lossy().into_owned(),
        ],
        cwd: fixture.temp.path().to_path_buf(),
        env: Default::default(),
        cols: 120,
        rows: 40,
    })
    .expect("healthy input capture runtime");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !ready.exists() {
        assert!(std::time::Instant::now() < deadline, "PTY not ready");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        super::super::actor_delivery::dispatch(
            &fixture.job.home,
            &fixture.job.group,
            &fixture.job.event
        )
        .queued,
        1
    );
    while !std::fs::read_to_string(&wire)
        .unwrap_or_default()
        .contains(marker)
    {
        assert!(std::time::Instant::now() < deadline, "payload not written");
        std::thread::sleep(Duration::from_millis(10));
    }
    // The payload was written, but the terminal's 1.5-second submit delay has not ended.
    let before = std::fs::read_to_string(&wire).expect("wire before Start");
    assert_eq!(before.matches(marker).count(), 1);
    let mut followup = Event::new("chat.message", &fixture.job.group.group_id);
    followup.by = "user".into();
    followup.data=serde_json::json!({"to":[fixture.job.actor.id],"text":"QUEUED_FOLLOWUP_5321","message_mode":"send"}).as_object().expect("followup payload").clone();
    cccc_core::ledger::append(
        &store
            .ledger_path(&fixture.job.group.group_id)
            .expect("ledger path"),
        &followup,
    )
    .expect("persist followup");
    assert_eq!(
        super::super::actor_delivery::dispatch(&fixture.job.home, &fixture.job.group, &followup)
            .queued,
        1
    );
    let request = DaemonRequest { v: 1, op: "actor_start".into(), args: serde_json::json!({"group_id":fixture.job.group.group_id,"actor_id":fixture.job.actor.id,"by":"user"}).as_object().expect("args").clone() };
    let response = crate::dispatch::dispatch(&fixture.job.home, &request);
    assert!(response.ok, "{response:?}");
    let current = cccc_runtime::status(&fixture.job.group.group_id, &fixture.job.actor.id)
        .expect("healthy status");
    assert!(current.running);
    assert_eq!(current.pid, healthy.pid);
    let accepted = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let state = super::super::runtime_delivery::latest_state(
            &fixture.job.home,
            &fixture.job.group.group_id,
            &fixture.job.actor.id,
            &fixture.job.event.id,
        )
        .expect("delivery state");
        if state.is_some_and(|state| state.0 == "accepted") {
            break;
        }
        assert!(
            std::time::Instant::now() < accepted,
            "message not accepted after Start"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if super::super::runtime_delivery::latest_state(
            &fixture.job.home,
            &fixture.job.group.group_id,
            &fixture.job.actor.id,
            &followup.id,
        )
        .expect("followup state")
        .is_some_and(|s| s.0 == "accepted")
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "queued followup was lost during Start"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let bytes = std::fs::read_to_string(&wire).expect("wire after acceptance");
    assert_eq!(bytes.matches("QUEUED_FOLLOWUP_5321").count(), 1);
    assert_eq!(bytes.matches("[CCCC] You are crasher").count(), 1);
    let events = cccc_core::ledger::read_all(
        &store
            .ledger_path(&fixture.job.group.group_id)
            .expect("ledger path"),
    )
    .expect("read ledger");
    for id in [&fixture.job.event.id, &followup.id] {
        assert_eq!(
            events
                .iter()
                .filter(|e| e.kind == "runtime.delivery"
                    && e.data["state"] == "accepted"
                    && e.data["source_event_id"] == *id)
                .count(),
            1
        );
    }
    assert_eq!(
        bytes.matches(marker).count(),
        1,
        "healthy Start repeated payload bytes in the same live terminal: {bytes:?}"
    );
}

#[test]
fn pending_wait_expires_on_running_runtime_without_launch_or_duplicate_input() {
    let mut fixture = Fixture::new();
    fixture.first_automatic_start();
    fixture
        .job
        .event
        .data
        .insert("text".into(), serde_json::json!("WAIT_EXPIRY_TASK_238dea"));
    fixture
        .job
        .event
        .data
        .insert("message_mode".into(), serde_json::json!("send"));
    let store = GroupStore::new(fixture.job.home.clone()).expect("store");
    let ledger_path = store
        .ledger_path(&fixture.job.group.group_id)
        .expect("ledger path");
    cccc_core::ledger::append(&ledger_path, &fixture.job.event).expect("durable message");
    assert_eq!(
        super::super::actor_delivery::dispatch(
            &fixture.job.home,
            &fixture.job.group,
            &fixture.job.event
        )
        .queued,
        1
    );
    std::thread::sleep(Duration::from_millis(250));
    let old_cancelled = super::super::actor_delivery::worker_cancellation(
        &fixture.job.group.group_id,
        &fixture.job.actor.id,
    )
    .expect("waiting worker");
    assert!(!old_cancelled.load(Ordering::Acquire));
    let wire = fixture.temp.path().join("wire");
    let ready = fixture.temp.path().join("ready");
    let healthy = cccc_runtime::start(cccc_runtime::LaunchSpec {
        group_id: fixture.job.group.group_id.clone(),
        actor_id: fixture.job.actor.id.clone(),
        runner: cccc_contracts::RunnerKind::Pty,
        command: vec![
            "sh".into(),
            "-c".into(),
            "stty raw -echo; touch \"$2\"; cat > \"$1\"".into(),
            "fixture".into(),
            wire.to_string_lossy().into_owned(),
            ready.to_string_lossy().into_owned(),
        ],
        cwd: fixture.temp.path().into(),
        env: Default::default(),
        cols: 120,
        rows: 40,
    })
    .expect("runtime became healthy outside the worker");
    let ready_deadline = std::time::Instant::now() + Duration::from_secs(3);
    while !ready.exists() {
        assert!(std::time::Instant::now() < ready_deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let request = DaemonRequest {v:1, op:"actor_start".into(), args:serde_json::json!({"group_id":fixture.job.group.group_id,"actor_id":fixture.job.actor.id,"by":"user"}).as_object().expect("args").clone()};
    let started = std::time::Instant::now();
    assert!(crate::dispatch::dispatch(&fixture.job.home, &request).ok);
    assert!(started.elapsed() < Duration::from_secs(1));
    let current_worker = super::super::actor_delivery::worker_cancellation(
        &fixture.job.group.group_id,
        &fixture.job.actor.id,
    )
    .expect("preserved worker");
    assert!(std::sync::Arc::ptr_eq(&old_cancelled, &current_worker));
    assert!(!old_cancelled.load(Ordering::Acquire));
    assert!(
        !std::fs::read_to_string(&wire)
            .unwrap_or_default()
            .contains("WAIT_EXPIRY_TASK_238dea"),
        "the old ten-second wait must still be pending"
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let accepted = super::super::runtime_delivery::latest_state(
            &fixture.job.home,
            &fixture.job.group.group_id,
            &fixture.job.actor.id,
            &fixture.job.event.id,
        )
        .expect("delivery state")
        .is_some_and(|state| state.0 == "accepted");
        let bytes = std::fs::read_to_string(&wire).unwrap_or_default();
        if accepted && bytes.contains("WAIT_EXPIRY_TASK_238dea") {
            assert_eq!(bytes.matches("WAIT_EXPIRY_TASK_238dea").count(), 1);
            assert_eq!(bytes.matches("[CCCC] You are crasher").count(), 1);
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "pending wait did not finish delivery"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let current = cccc_runtime::status(&fixture.job.group.group_id, &fixture.job.actor.id)
        .expect("healthy status");
    assert_eq!(current.pid, healthy.pid);
    assert_eq!(current.started_at, healthy.started_at);
    assert_eq!(
        fixture.launches(),
        1,
        "expiry must not relaunch the configured command"
    );
    assert_eq!(
        super::super::actor_delivery::dispatch(
            &fixture.job.home,
            &fixture.job.group,
            &fixture.job.event
        )
        .queued,
        0,
        "accepted input must not be queued twice"
    );
    let events = cccc_core::ledger::read_all(&ledger_path).expect("ledger");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == "runtime.delivery"
                && event.data["state"] == "accepted"
                && event.data["source_event_id"] == fixture.job.event.id)
            .count(),
        1
    );
    assert!(
        super::super::actor_respawn_backoff::begin_restart(
            &fixture.job.group.group_id,
            &fixture.job.actor.id
        )
        .is_zero(),
        "the expired wait must not revive the history cleared by Start"
    );
    super::super::actor_respawn_backoff::forget(&fixture.job.group.group_id, &fixture.job.actor.id);
}
