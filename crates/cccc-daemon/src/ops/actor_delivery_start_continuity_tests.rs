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
