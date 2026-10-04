use cccc_contracts::{DaemonRequest, DaemonResponse, Event};
use cccc_core::{GroupStore, HomeLayout, ledger, mail_attention};
use serde_json::{Value, json};

struct Fixture {
    _temp: tempfile::TempDir,
    home: HomeLayout,
    group_id: String,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("temp");
        let home = HomeLayout::from_path(temp.path()).expect("home");
        let created = call(&home, "group_create", json!({"title":"attention"}));
        let group_id = created.result["group"]["group_id"]
            .as_str()
            .expect("gid")
            .to_owned();
        call(
            &home,
            "actor_add",
            json!({"group_id":group_id,"actor_id":"web1","runtime":"web_model","by":"user"}),
        );
        GroupStore::new(home.clone())
            .expect("store")
            .mutate(&group_id, |g| {
                g.running = true;
                Ok(())
            })
            .expect("running");
        Self {
            _temp: temp,
            home,
            group_id,
        }
    }
    fn path(&self) -> std::path::PathBuf {
        GroupStore::new(self.home.clone())
            .expect("store")
            .ledger_path(&self.group_id)
            .expect("path")
    }
    fn mail(&self) -> String {
        let mut event = Event::new("chat.message", &self.group_id);
        event.by = "sender".into();
        event.ts = (chrono::Utc::now() - chrono::Duration::minutes(6)).to_rfc3339();
        event.data = json!({"message_mode":"mail","to":["web1"],"text":"private source"})
            .as_object()
            .cloned()
            .expect("data");
        let id = event.id.clone();
        ledger::append(&self.path(), &event).expect("append");
        id
    }
    fn pull(&self) -> DaemonResponse {
        call(
            &self.home,
            "runtime_wait_next_turn",
            json!({"group_id":self.group_id,"actor_id":"web1","by":"web1"}),
        )
    }
}

#[test]
fn boundary_attention_is_content_free_bounded_and_never_replayed() {
    let f = Fixture::new();
    let source = f.mail();
    let turn = f.pull();
    assert_eq!(turn.result["status"], "work_available");
    assert_eq!(
        turn.result["turn"]["messages"][0]["data"]["kind"],
        "mail_attention"
    );
    assert!(
        !turn.result["turn"]["coalesced_text"]
            .as_str()
            .expect("text")
            .contains("private source")
    );
    let summary =
        mail_attention::inspect(&f.home, &f.group_id, "web1", chrono::Utc::now().timestamp())
            .expect("summary");
    assert_eq!(summary.state.standalone_wakeups, 1);
    assert_eq!(summary.unread_count, 1);
    assert_eq!(f.pull().result["status"], "turn_in_progress");
    let active = call(
        &f.home,
        "mail_attention_context",
        json!({"group_id":f.group_id,"actor_id":"web1","by":"web1","carrier_id":"inside-reminder"}),
    );
    assert!(active.result["mail_pending"].is_null());
    let recovery = raw(
        &f.home,
        "web_model_runtime_recover_turn",
        json!({"group_id":f.group_id,"actor_id":"web1","by":"web1","event_ids":turn.result["turn"]["event_ids"]}),
    );
    assert_eq!(
        recovery.error.expect("error").code,
        "attention_not_replayable"
    );
    call(
        &f.home,
        "runtime_complete_turn",
        json!({"group_id":f.group_id,"actor_id":"web1","by":"web1","status":"done","event_ids":turn.result["turn"]["event_ids"]}),
    );
    assert_eq!(f.pull().result["status"], "idle");
    assert!(
        ledger::read_all(&f.path())
            .expect("events")
            .iter()
            .all(|e| e.kind != "mail.read")
    );
    assert!(
        cccc_core::inbox::cursor(&f.home, &f.group_id, "web1")
            .expect("cursor")
            .is_none()
    );
    assert!(
        ledger::read_all(&f.path())
            .expect("events")
            .iter()
            .any(|e| e.id == source)
    );
}

#[test]
fn ordinary_work_wins_and_consumes_only_the_shared_passive_token() {
    let f = Fixture::new();
    f.mail();
    call(
        &f.home,
        "send",
        json!({"group_id":f.group_id,"by":"user","to":["web1"],"text":"ordinary","message_mode":"send"}),
    );
    let turn = f.pull();
    assert_eq!(
        turn.result["turn"]["messages"]
            .as_array()
            .expect("messages")
            .len(),
        1
    );
    assert_eq!(turn.result["turn"]["messages"][0]["kind"], "chat.message");
    assert!(
        turn.result["turn"]["coalesced_text"]
            .as_str()
            .expect("text")
            .contains("MAIL PENDING: 1 item")
    );
    let s = mail_attention::inspect(&f.home, &f.group_id, "web1", chrono::Utc::now().timestamp())
        .expect("summary");
    assert_eq!(s.state.standalone_wakeups, 0);
    assert_eq!(s.state.attempt_no, 1);
    let mcp = call(
        &f.home,
        "mail_attention_context",
        json!({"group_id":f.group_id,"actor_id":"web1","by":"web1","carrier_id":"mcp"}),
    );
    assert!(mcp.result["mail_pending"].is_null());
}

#[test]
fn context_ownership_and_paused_group_fail_closed_without_new_notifications() {
    let f = Fixture::new();
    f.mail();
    for by in ["user", "other"] {
        let r = raw(
            &f.home,
            "mail_attention_context",
            json!({"group_id":f.group_id,"actor_id":"web1","by":by,"carrier_id":"foreign"}),
        );
        assert_eq!(r.error.expect("error").code, "permission_denied");
    }
    GroupStore::new(f.home.clone())
        .expect("store")
        .mutate(&f.group_id, |g| {
            g.state = cccc_contracts::GroupState::Paused;
            Ok(())
        })
        .expect("pause");
    assert_eq!(f.pull().result["status"], "stopped");
    let context = call(
        &f.home,
        "mail_attention_context",
        json!({"group_id":f.group_id,"actor_id":"web1","by":"web1","carrier_id":"paused"}),
    );
    assert!(context.result["mail_pending"].is_null());
    assert!(
        ledger::read_all(&f.path())
            .expect("events")
            .iter()
            .all(|e| e.kind != "system.notify")
    );
}

#[test]
fn expired_mail_is_preserved_but_absent_from_every_automatic_context() {
    let f = Fixture::new();
    let mut event = Event::new("chat.message", &f.group_id);
    event.by = "sender".into();
    event.ts = (chrono::Utc::now() - chrono::Duration::hours(72)).to_rfc3339();
    event.data = json!({"message_mode":"mail","to":["web1"]})
        .as_object()
        .cloned()
        .expect("data");
    ledger::append(&f.path(), &event).expect("append");
    assert_eq!(f.pull().result["status"], "idle");
    let c = call(
        &f.home,
        "mail_attention_context",
        json!({"group_id":f.group_id,"actor_id":"web1","by":"web1","carrier_id":"expired"}),
    );
    assert!(c.result["mail_pending"].is_null());
    let s = call(
        &f.home,
        "mail_attention_status",
        json!({"group_id":f.group_id,"actor_id":"web1","by":"web1"}),
    );
    assert_eq!(s.result["mail_attention"]["unread_count"], 1);
    assert_eq!(s.result["mail_attention"]["attention_count"], 0);
    assert_eq!(s.result["mail_attention"]["expired_unread_count"], 1);
}
fn raw(home: &HomeLayout, op: &str, args: Value) -> DaemonResponse {
    cccc_daemon::handle_request(
        home,
        &DaemonRequest {
            v: 1,
            op: op.into(),
            args: args.as_object().cloned().expect("args"),
        },
    )
}
fn call(home: &HomeLayout, op: &str, args: Value) -> DaemonResponse {
    let r = raw(home, op, args);
    assert!(r.ok, "{op}: {:?}", r.error);
    r
}

#[test]
fn browser_handoff_needs_its_own_atomic_adapter_before_standalone_attention() {
    let f = Fixture::new();
    f.mail();
    let browser = call(
        &f.home,
        "runtime_wait_next_turn",
        json!({"group_id":f.group_id,"actor_id":"web1","by":"web1","transport":"web_model_browser"}),
    );
    assert_eq!(browser.result["status"], "idle");
    assert!(
        ledger::read_all(&f.path())
            .expect("events")
            .iter()
            .all(|e| e.kind != "system.notify")
    );
    assert_eq!(f.pull().result["status"], "work_available");
}
