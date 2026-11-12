//! Replay retained Mail through real native admission; hold metadata is a
//! labelled protocol control, not claimed historical browser evidence.
use super::*;
use cccc_contracts::DaemonRequest;

fn fixture() -> (tempfile::TempDir, HomeLayout, GroupDoc, Value) {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    let store = GroupStore::new(home.clone()).expect("store");
    let mut group = store.create("", "").expect("test envelope");
    let actor: Actor = serde_json::from_str(include_str!(
        "../../../cccc-core/tests/fixtures/web-model-pin/actor.json"
    ))
    .expect("captured Actor");
    let actor = cccc_core::actors::add(&mut group, actor).expect("add");
    group.running = true;
    store.save(&group).expect("save");
    let mut history: Vec<Event> = serde_json::from_str(include_str!(
        "../../../cccc-core/tests/fixtures/mail_notice_history.json"
    ))
    .expect("retained real Mail history");
    let at = chrono::DateTime::parse_from_rfc3339(&history[1].ts).expect("capture time");
    let shift = chrono::Utc::now().signed_duration_since(at);
    // Retained Mail + its notice: consistent aliases and one timeline shift
    // preserve unread Mail and native notification admission.
    let path = store.ledger_path(&group.group_id).expect("ledger");
    for event in &mut history[..2] {
        event.group_id = group.group_id.clone();
        event.ts =
            (chrono::DateTime::parse_from_rfc3339(&event.ts).expect("time") + shift).to_rfc3339();
        if event.kind == "chat.message" {
            event.data.insert("to".into(), json!([actor.id]));
        } else {
            event.data.insert("target_actor_id".into(), json!(actor.id));
            event.data.get_mut("context").expect("notice context")["actor_id"] = json!(actor.id);
            event.data.get_mut("context").expect("notice context")["actor_created_at"] =
                json!(actor.created_at);
        }
        ledger::append(&path, event).expect("append capture");
    }
    let admitted = wait(&home, &group, None);
    assert_eq!(admitted["status"], "work_available");
    let mut group = store.load(&group.group_id).expect("native state");
    let turn = &admitted["turn"];
    let target = json!({
        "last_delivery_status":"model_pin_held","binding_revision":"binding-control","url":"https://chatgpt.com/c/redacted-control",
        "last_delivery_turn_id":turn["turn_id"],"last_delivery_event_ids":turn["event_ids"],"last_delivery_id":"delivery-control",
        "last_submission_evidence":{"submitted":false,"submission_evidence":"model_pin_unavailable"},
        "model_pin_hold":{"v":1,"legacy":false,"actor_generation":cccc_core::actors::generation_identity(&actor),
            "policy_revision":actor.web_model_pin_revision,"binding_revision":"binding-control","url":"https://chatgpt.com/c/redacted-control",
            "turn_id":turn["turn_id"],"event_ids":turn["event_ids"],"delivery_id":"delivery-control",
            "prompt_sha256":format!("{:x}",Sha256::digest(history[1].data["message"].as_str().expect("captured notice").as_bytes()))}
    });
    group.extra.insert(
        "web_model_browser_targets".into(),
        json!({"browser":target}),
    );
    store.save(&group).expect("durable unsent hold control");
    (temp, home, group, admitted)
}

fn wait(home: &HomeLayout, group: &GroupDoc, resume: Option<&str>) -> Value {
    let response=crate::handle_request(home,&DaemonRequest {
        v:1,op:"runtime_wait_next_turn".into(),
        args:json!({"group_id":group.group_id,"actor_id":"browser","transport":"web_model_browser","resume_turn_id":resume})
            .as_object().expect("args").clone()
    });
    assert!(response.ok, "{:?}", response.error);
    json!(response.result)
}

#[test]
fn known_unsent_hold_survives_restart_and_resumes_the_exact_native_batch() {
    let (_temp, home, group, admitted) = fixture();
    assert_eq!(settle_stranded_claims(&home, &group).expect("restart"), 0);
    let resumed = wait(&home, &group, admitted["turn"]["turn_id"].as_str());
    assert_eq!(resumed["status"], "work_available");
    assert_eq!(resumed["turn"]["turn_id"], admitted["turn"]["turn_id"]);
    assert_eq!(resumed["turn"]["event_ids"], admitted["turn"]["event_ids"]);
    assert_eq!(
        latest_state(
            &home,
            &group.group_id,
            "browser",
            admitted["turn"]["event_ids"][0].as_str().expect("source")
        )
        .expect("state"),
        Some(("claimed".into(), "web_model_browser".into()))
    );
}

#[test]
fn hold_marker_never_reopens_a_terminal_source() {
    let (_temp, home, group, admitted) = fixture();
    let actor = &group.actors[0];
    let event = admitted["turn"]["event_ids"][0].as_str().expect("source");
    append_state(
        &home,
        &group.group_id,
        "browser",
        &actor.created_at,
        event,
        "web_model_browser",
        DeliveryOutcome::Accepted,
    )
    .expect("actual terminal transition control");
    assert_eq!(settle_stranded_claims(&home, &group).expect("restart"), 0);
    let resumed = wait(&home, &group, admitted["turn"]["turn_id"].as_str());
    assert_eq!(resumed["status"], "turn_in_progress");
    assert_ne!(resumed["status"], "work_available");
}

#[test]
fn stale_or_dispatched_markers_keep_existing_ambiguous_restart_fence() {
    for field in [
        "actor_generation",
        "policy_revision",
        "binding_revision",
        "turn_id",
        "event_ids",
        "delivery_id",
        "dispatched",
    ] {
        let (_temp, home, mut group, _admitted) = fixture();
        let target = &mut group
            .extra
            .get_mut("web_model_browser_targets")
            .expect("targets")["browser"];
        if field == "dispatched" {
            target["last_delivery_status"] = json!("submitting");
        } else if field == "policy_revision" {
            target["model_pin_hold"][field] = json!(99);
        } else if field == "event_ids" {
            target["model_pin_hold"][field] = json!([]);
        } else {
            target["model_pin_hold"][field] = json!("stale-control");
        }
        GroupStore::new(home.clone())
            .expect("store")
            .save(&group)
            .expect("fault control");
        assert_eq!(
            settle_stranded_claims(&home, &group).expect("restart"),
            1,
            "{field}"
        );
    }
}
