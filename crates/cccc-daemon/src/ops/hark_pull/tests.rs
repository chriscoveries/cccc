use super::*;
use cccc_contracts::{DaemonResponse, Event};

struct Fixture {
    _temp: tempfile::TempDir,
    home: HomeLayout,
    group: GroupDoc,
    id: String,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("isolated Hark fixture");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        home.initialize().expect("initialize");
        let groups = GroupStore::new(home.clone()).expect("groups");
        let mut group = groups.create("Hark fixture", "").expect("group");
        let mut actor = Actor::new("hark");
        actor.runtime = ActorRuntime::Hark;
        actor.runner = RunnerKind::Headless;
        actor.enabled = false;
        cccc_core::actors::add(&mut group, actor).expect("Hark");
        let mut dot = Actor::new("hhdot");
        dot.enabled = false;
        cccc_core::actors::add(&mut group, dot).expect("named peer");
        group.running = true;
        group.state = GroupState::Active;
        groups.save(&group).expect("save");
        let created = private::configure(
            &home,
            &group.group_id,
            "hark",
            &cccc_core::actors::generation_identity(&group.actors[0]),
            false,
        )
        .expect("synthetic connector");
        Self {
            _temp: temp,
            home,
            group,
            id: created["connector"]["connector_id"]
                .as_str()
                .expect("connector ID")
                .into(),
        }
    }

    fn response(&self, op: &str, mut args: Value) -> DaemonResponse {
        let m = args.as_object_mut().expect("args");
        m.entry("group_id").or_insert(json!(self.group.group_id));
        m.entry("actor_id").or_insert(json!("hark"));
        m.entry("by").or_insert(json!("user"));
        crate::dispatch::dispatch(
            &self.home,
            &DaemonRequest {
                v: 1,
                op: op.into(),
                args: m.clone(),
            },
        )
    }
    fn call(&self, op: &str, args: Value) -> Value {
        let r = self.response(op, args);
        assert!(r.ok, "{op}: {:?}", r.error);
        Value::Object(r.result)
    }
    fn start(&self) {
        self.call("actor_start", json!({}));
    }
    fn private(&self) -> Value {
        store::load(&self.home)
            .expect("connectors")
            .into_iter()
            .find(|c| c["connector_id"] == self.id)
            .expect("connector")
    }
    fn binding(&self, token: &str, tool: &str) -> Value {
        let mut b = private::resolve(&self.private(), token, tool).expect("binding");
        b["connector_id"] = json!(self.id);
        b
    }
    fn poll(&self) -> Value {
        self.claim(&self.binding("", "cccc_runtime_wait_next_turn"))
    }
    fn claim(&self, b: &Value) -> Value {
        self.call(
            "hark_work_claim",
            json!({"by":"hark","_cccc_web_binding":b}),
        )
    }
    fn source(&self, text: &str) -> String {
        self.call("send", json!({"to":["@hark"],"text":text}))["event"]["id"]
            .as_str()
            .expect("source event")
            .into()
    }
    fn execution(&self, c: &Value, tool: &str) -> Value {
        self.binding(
            c["actor_token"].as_str().expect("execution credential"),
            tool,
        )
    }
    fn complete(&self, c: &Value) -> Value {
        self.call("runtime_complete_turn", json!({"by":"hark","turn_id":c["turn"]["turn_id"],
            "event_ids":c["turn"]["event_ids"],"_cccc_web_binding":self.execution(c,"cccc_runtime_complete_turn")}))
    }
    fn ledger(&self) -> Vec<Event> {
        let path = GroupStore::new(self.home.clone())
            .expect("groups")
            .ledger_path(&self.group.group_id)
            .expect("ledger path");
        std::fs::read_to_string(path)
            .expect("ledger")
            .lines()
            .map(|line| serde_json::from_str(line).expect("event"))
            .collect()
    }
}

#[test]
fn owner_mention_is_pulled_replied_in_thread_and_completed() {
    let f = Fixture::new();
    f.start();
    let source = f.source("@hark please review this plan");
    let claimed = f.poll();
    assert_eq!(claimed["status"], "work_available");
    assert_eq!(claimed["turn"]["event_ids"], json!([source]));
    assert_eq!(claimed["turn"]["messages"][0]["by"], "user");
    assert!(claimed["turn"]["system_prompt"].is_string());
    assert!(
        claimed["turn"]["coalesced_text"]
            .as_str()
            .expect("materials")
            .contains("review this plan")
    );
    let reply = f.call(
        "reply",
        json!({"by":"hark","reply_to":source,"text":"Reviewed.",
        "_cccc_web_binding":f.execution(&claimed,"cccc_message_reply")}),
    );
    assert_eq!(reply["event"]["by"], "hark");
    assert_eq!(reply["event"]["data"]["reply_to"], source);
    assert_eq!(f.complete(&claimed)["status"], "done");
    assert_eq!(f.poll()["status"], "idle");
    assert_eq!(
        f.call("hark_pull_status", json!({}))["last_execution"]["status"],
        "done"
    );
}

#[test]
fn execution_can_dispatch_to_a_named_actor_without_browser() {
    let f = Fixture::new();
    f.start();
    f.source("Delegate the implementation to @hhdot");
    let claimed = f.poll();
    let sent = f.call(
        "send",
        json!({"by":"hark","to":["@hhdot"],"text":"Please implement the plan.",
        "_cccc_web_binding":f.execution(&claimed,"cccc_message_send")}),
    );
    assert_eq!(sent["event"]["by"], "hark");
    assert_eq!(sent["event"]["data"]["to"], json!(["hhdot"]));
    f.complete(&claimed);
}

#[test]
fn idle_poll_is_immediate_and_records_diagnostics_without_a_turn() {
    let f = Fixture::new();
    f.start();
    let start = std::time::Instant::now();
    let idle = f.poll();
    assert!(
        start.elapsed() < std::time::Duration::from_secs(2),
        "poll must not long-hold"
    );
    assert_eq!(idle["status"], "idle");
    assert_eq!(idle["reason_code"], "hark_no_work");
    assert!(idle["actor_token"].is_null());
    let status = f.call("hark_pull_status", json!({}));
    assert!(status["last_poll_at"].is_string());
    assert_eq!(status["last_poll_status"], "idle");
    assert!(status["last_claim_at"].is_null());
}

#[test]
fn duplicate_poll_never_replays_an_unfinished_turn_even_after_restart() {
    let f = Fixture::new();
    f.start();
    let source = f.source("one turn only");
    let binding = f.binding("", "cccc_runtime_wait_next_turn");
    let first = f.claim(&binding);
    let duplicate = f.claim(&binding);
    assert_eq!(duplicate["status"], "turn_in_progress");
    assert_eq!(duplicate["active_turn_id"], first["turn"]["turn_id"]);
    assert!(duplicate["turn"].is_null());
    assert!(duplicate["actor_token"].is_null());
    // The fixture reloads disk state for every call; stopping also retires the
    // original execution credential without returning its task to the queue.
    f.call("actor_stop", json!({}));
    f.start();
    assert_eq!(f.poll()["status"], "turn_in_progress");
    let status = f.call("hark_pull_status", json!({}));
    assert_eq!(status["phase"], "interrupted");
    assert_eq!(status["active_event_ids"], json!([source]));
    let claims = f
        .ledger()
        .iter()
        .filter(|e| e.kind == "runtime.delivery" && e.data.get("state") == Some(&json!("claimed")))
        .count();
    assert_eq!(claims, 1);
}

#[test]
fn concurrent_polls_claim_once() {
    let f = Fixture::new();
    f.start();
    f.source("claim concurrently");
    let binding = f.binding("", "cccc_runtime_wait_next_turn");
    let results = std::thread::scope(|scope| {
        let jobs = (0..4)
            .map(|_| scope.spawn(|| f.claim(&binding)))
            .collect::<Vec<_>>();
        jobs.into_iter()
            .map(|job| job.join().expect("poll thread"))
            .collect::<Vec<_>>()
    });
    assert_eq!(
        results
            .iter()
            .filter(|r| r["status"] == "work_available")
            .count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|r| r["status"] == "turn_in_progress")
            .count(),
        3
    );
}

#[test]
fn stopped_actor_or_group_never_claims() {
    let f = Fixture::new();
    let source = f.source("wait until started");
    assert_eq!(f.poll()["status"], "stopped");
    assert!(
        super::super::runtime_delivery::latest_state(&f.home, &f.group.group_id, "hark", &source)
            .expect("state")
            .is_none()
    );
    f.start();
    let groups = GroupStore::new(f.home.clone()).expect("groups");
    let mut g = groups.load(&f.group.group_id).expect("group");
    g.state = GroupState::Paused;
    groups.save(&g).expect("pause");
    assert_eq!(f.poll()["status"], "stopped");
    g.state = GroupState::Active;
    groups.save(&g).expect("resume");
    assert_eq!(f.poll()["status"], "work_available");
}

#[test]
fn claim_authority_cannot_reply_complete_or_bypass_the_connector() {
    let f = Fixture::new();
    f.start();
    let source = f.source("protected work");
    let b = f.binding("", "cccc_runtime_wait_next_turn");
    for (op, args) in [
        ("send", json!({"text":"forbidden","to":["hhdot"]})),
        ("reply", json!({"text":"forbidden","reply_to":source})),
        (
            "runtime_complete_turn",
            json!({"turn_id":"fake","event_ids":[source]}),
        ),
    ] {
        let mut args = args;
        args["by"] = json!("hark");
        args["_cccc_web_binding"] = b.clone();
        assert_eq!(
            f.response(op, args).error.expect("denied").code,
            "permission_denied"
        );
    }
    assert_eq!(
        f.response("runtime_wait_next_turn", json!({"by":"hark"}))
            .error
            .expect("connector required")
            .code,
        "hark_connector_required"
    );
    assert_eq!(
        f.response(
            "web_model_runtime_recover_turn",
            json!({"by":"hark","event_ids":[source]})
        )
        .error
        .expect("no replay")
        .code,
        "hark_explicit_resolution_required"
    );
}

#[test]
fn completion_is_idempotent_and_credential_is_scoped_to_original_turn() {
    let f = Fixture::new();
    f.start();
    f.source("first");
    let first = f.poll();
    let completed = f.complete(&first);
    assert_eq!(
        f.complete(&first)["completion_event"]["id"],
        completed["completion_event"]["id"]
    );
    let old = f.execution(&first, "cccc_runtime_complete_turn");
    f.source("second");
    let second = f.poll();
    let rejected = f.response(
        "runtime_complete_turn",
        json!({"by":"hark","turn_id":second["turn"]["turn_id"],
        "event_ids":second["turn"]["event_ids"],"_cccc_web_binding":old}),
    );
    assert_eq!(
        rejected.error.expect("old turn denied").code,
        "permission_denied"
    );
    f.complete(&first);
    assert_eq!(
        f.call("hark_pull_status", json!({}))["active_turn_id"],
        second["turn"]["turn_id"]
    );
    let wrong = f.response("runtime_complete_turn",json!({"by":"hark","turn_id":second["turn"]["turn_id"],
        "event_ids":first["turn"]["event_ids"],"_cccc_web_binding":f.execution(&second,"cccc_runtime_complete_turn")}));
    assert_eq!(
        wrong.error.expect("exact event IDs required").code,
        "completion_conflict"
    );
    f.complete(&second);
}

#[test]
fn rotation_and_explicit_resolution_preserve_unfinished_work_without_replay() {
    let f = Fixture::new();
    f.start();
    let old_binding = f.binding("", "cccc_runtime_wait_next_turn");
    let source = f.source("interrupted work");
    let claimed = f.poll();
    let execution = f.execution(&claimed, "cccc_message_send");
    f.call("hark_connector_configure", json!({"rotate":true}));
    assert!(store::validate_binding(&f.home, &old_binding).is_err());
    assert!(store::validate_binding(&f.home, &execution).is_err());
    assert_eq!(f.poll()["status"], "turn_in_progress");
    assert_eq!(
        f.call("hark_pull_status", json!({}))["phase"],
        "interrupted"
    );
    let args = json!({"turn_id":claimed["turn"]["turn_id"],"event_ids":[source]});
    let mut forbidden = args.clone();
    forbidden["by"] = json!("hark");
    assert_eq!(
        f.response("hark_work_resolve", forbidden)
            .error
            .expect("owner only")
            .code,
        "permission_denied"
    );
    assert_eq!(f.call("hark_work_resolve", args)["status"], "cancelled");
    assert_eq!(f.poll()["status"], "idle");
    f.source("fresh work");
    assert_eq!(f.poll()["status"], "work_available");
}

#[test]
fn canonical_turn_prevents_replay_if_private_projection_is_lost() {
    let f = Fixture::new();
    f.start();
    f.source("durable owner");
    let first = f.poll();
    private::change(&f.home, &f.id, &f.group.group_id, "hark", |_, s| {
        s["execution"] = Value::Null;
        Ok(())
    })
    .expect("simulate interrupted projection");
    let after = f.poll();
    assert_eq!(after["status"], "turn_in_progress");
    assert_eq!(after["active_turn_id"], first["turn"]["turn_id"]);
    assert!(after["actor_token"].is_null());
    assert_eq!(
        f.call("hark_pull_status", json!({}))["phase"],
        "interrupted"
    );
}

#[test]
fn execution_credentials_do_not_enter_public_ledger_or_status() {
    let f = Fixture::new();
    f.start();
    f.source("private credential");
    let claimed = f.poll();
    let token = claimed["actor_token"].as_str().expect("token");
    assert!(
        !serde_json::to_string(&f.ledger())
            .expect("ledger")
            .contains(token)
    );
    assert!(
        !f.call("hark_pull_status", json!({}))
            .to_string()
            .contains(token)
    );
}
