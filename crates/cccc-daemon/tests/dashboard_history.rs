#![cfg(unix)]
use cccc_contracts::{DaemonRequest, Event};
use cccc_core::{GroupStore, HomeLayout};
use serde_json::{Value, json};
use std::io::Write;

fn call(home: &HomeLayout, op: &str, args: Value) -> Value {
    let response = cccc_daemon::handle_request(
        home,
        &DaemonRequest {
            v: 1,
            op: op.into(),
            args: args.as_object().expect("valid dashboard fixture").clone(),
        },
    );
    assert!(response.ok, "{op}: {:?}", response.error);
    Value::Object(response.result)
}

#[test]
fn dashboard_pages_and_statuses_preserve_chat_order_over_telemetry() {
    let temp = tempfile::tempdir().expect("valid dashboard fixture");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("valid dashboard fixture");
    home.initialize().expect("valid dashboard fixture");
    let group = GroupStore::new(home.clone())
        .expect("valid dashboard fixture")
        .create("history", "")
        .expect("valid dashboard fixture");
    let path = GroupStore::new(home.clone())
        .expect("valid dashboard fixture")
        .ledger_path(&group.group_id)
        .expect("valid dashboard fixture");
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("valid dashboard fixture");
    for n in 0..60 {
        for kind in ["actor.activity", "runtime.delivery", "chat.message"] {
            let mut event = Event::new(kind, &group.group_id);
            event.id = format!("{kind}-{n}");
            event.by = "user".into();
            event.data = json!({"text":format!("message {n}"),"to":["user"]})
                .as_object()
                .expect("valid dashboard fixture")
                .clone();
            writeln!(
                file,
                "{}",
                serde_json::to_string(&event).expect("valid dashboard fixture")
            )
            .expect("valid dashboard fixture");
        }
    }
    drop(file);
    let tail = call(
        &home,
        "ledger_tail",
        json!({"group_id":group.group_id,"kind":"chat","limit":10,"with_read_status":true,"with_obligation_status":true}),
    );
    assert_eq!(
        tail["events"]
            .as_array()
            .expect("valid dashboard fixture")
            .len(),
        10
    );
    assert_eq!(tail["events"][0]["id"], "chat.message-50");
    let older = call(
        &home,
        "ledger_search",
        json!({"group_id":group.group_id,"kind":"chat","before":"chat.message-50","limit":10,"with_obligation_status":true}),
    );
    assert_eq!(older["events"][0]["id"], "chat.message-40");
    assert_eq!(older["events"][9]["id"], "chat.message-49");
    assert_eq!(older["has_more"], true);
    let window = call(
        &home,
        "ledger_window",
        json!({"group_id":group.group_id,"kind":"chat","center":"chat.message-30","before":2,"after":2,"with_obligation_status":true}),
    );
    assert_eq!(window["events"][0]["id"], "chat.message-28");
    assert_eq!(window["events"][4]["id"], "chat.message-32");
    assert_eq!(window["center_index"], 2);
    assert_eq!(window["has_more_before"], true);
    assert_eq!(window["has_more_after"], true);
}

#[test]
fn dashboard_status_keeps_last_valid_delivery_when_later_record_is_invalid() {
    let temp = tempfile::tempdir().expect("valid dashboard fixture");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("valid dashboard fixture");
    home.initialize().expect("valid dashboard fixture");
    let group = call(&home, "group_create", json!({"title":"delivery status"}));
    let group_id = group["group"]["group_id"]
        .as_str()
        .expect("valid dashboard fixture");
    call(
        &home,
        "group_stop",
        json!({"group_id":group_id,"by":"user"}),
    );
    call(
        &home,
        "actor_add",
        json!({"group_id":group_id,"actor_id":"peer","by":"user"}),
    );
    let path = GroupStore::new(home.clone())
        .expect("valid dashboard fixture")
        .ledger_path(group_id)
        .expect("valid dashboard fixture");
    let mut source = Event::new("chat.message", group_id);
    source.by = "user".into();
    source.data = json!({"text":"request","to":["peer"],"message_mode":"request_reply"})
        .as_object()
        .expect("valid dashboard fixture")
        .clone();
    cccc_core::ledger::append(&path, &source).expect("valid dashboard fixture");
    for state in [json!("accepted"), Value::Null] {
        let mut delivery = Event::new("runtime.delivery", group_id);
        delivery.data = json!({"source_event_id":source.id,"actor_id":"peer","state":state})
            .as_object()
            .expect("valid dashboard fixture")
            .clone();
        cccc_core::ledger::append(&path, &delivery).expect("valid dashboard fixture");
    }
    let status = call(
        &home,
        "ledger_statuses",
        json!({"group_id":group_id,"event_ids":[source.id]}),
    );
    assert_eq!(
        status["statuses"][&source.id]["obligation_status"]["peer"]["delivery_state"],
        "accepted"
    );
}

#[cfg(target_os = "linux")]
fn high_water_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .expect("valid dashboard fixture")
        .lines()
        .find_map(|line| {
            line.strip_prefix("VmHWM:").map(|value| {
                value
                    .split_whitespace()
                    .next()
                    .expect("valid dashboard fixture")
                    .parse()
                    .expect("valid dashboard fixture")
            })
        })
        .expect("valid dashboard fixture")
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "Run alone to measure fresh-process dashboard peak RSS"]
fn dashboard_big_ledger_has_bounded_peak_rss() {
    let temp = tempfile::tempdir().expect("valid dashboard fixture");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("valid dashboard fixture");
    home.initialize().expect("valid dashboard fixture");
    let group = GroupStore::new(home.clone())
        .expect("valid dashboard fixture")
        .create("large history", "")
        .expect("valid dashboard fixture");
    let path = GroupStore::new(home.clone())
        .expect("valid dashboard fixture")
        .ledger_path(&group.group_id)
        .expect("valid dashboard fixture");
    let mut file = std::io::BufWriter::new(
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("valid dashboard fixture"),
    );
    let padding = "x".repeat(2048);
    for n in 0..50_000 {
        let mut event = Event::new(
            if n % 2 == 0 {
                "actor.activity"
            } else {
                "runtime.delivery"
            },
            &group.group_id,
        );
        event.id = format!("telemetry-{n}");
        event.data = json!({"padding":padding,"source_event_id":"unrelated","actor_id":"absent","state":"accepted"}).as_object().expect("valid dashboard fixture").clone();
        writeln!(
            file,
            "{}",
            serde_json::to_string(&event).expect("valid dashboard fixture")
        )
        .expect("valid dashboard fixture");
    }
    for n in 0..200 {
        let mut event = Event::new("chat.message", &group.group_id);
        event.id = format!("chat-{n}");
        event.by = "user".into();
        event.data = json!({"text":format!("message {n}"),"to":["user"]})
            .as_object()
            .expect("valid dashboard fixture")
            .clone();
        writeln!(
            file,
            "{}",
            serde_json::to_string(&event).expect("valid dashboard fixture")
        )
        .expect("valid dashboard fixture");
    }
    drop(file);
    let baseline = high_water_kib();
    let started = std::time::Instant::now();
    let tail = call(
        &home,
        "ledger_tail",
        json!({"group_id":group.group_id,"kind":"chat","limit":50,"with_read_status":true,"with_obligation_status":true}),
    );
    eprintln!(
        "after_tail_kib={} elapsed_ms={}",
        high_water_kib(),
        started.elapsed().as_millis()
    );
    assert_eq!(
        tail["events"]
            .as_array()
            .expect("valid dashboard fixture")
            .len(),
        50
    );
    let page = call(
        &home,
        "ledger_search",
        json!({"group_id":group.group_id,"kind":"chat","before":"chat-150","limit":50,"with_read_status":true,"with_obligation_status":true}),
    );
    eprintln!(
        "after_page_kib={} elapsed_ms={}",
        high_water_kib(),
        started.elapsed().as_millis()
    );
    assert_eq!(page["events"][0]["id"], "chat-100");
    let window = call(
        &home,
        "ledger_window",
        json!({"group_id":group.group_id,"kind":"chat","center":"chat-75","before":10,"after":10,"with_obligation_status":true}),
    );
    assert_eq!(
        window["events"]
            .as_array()
            .expect("valid dashboard fixture")
            .len(),
        21
    );
    eprintln!(
        "after_window_kib={} elapsed_ms={}",
        high_water_kib(),
        started.elapsed().as_millis()
    );
    for response in [&tail, &page, &window] {
        assert!(
            response["events"]
                .as_array()
                .expect("returned page")
                .iter()
                .all(|event| event["_obligation_status"].is_object()),
            "history responses must retain status hydration"
        );
    }
    let replay =
        cccc_core::ledger::events_after(&path, "chat-190", 5).expect("valid dashboard fixture");
    assert_eq!(replay.len(), 5);
    let peak = high_water_kib();
    eprintln!(
        "ledger_bytes={} baseline_kib={baseline} peak_kib={peak} delta_kib={}",
        std::fs::metadata(path)
            .expect("valid dashboard fixture")
            .len(),
        peak.saturating_sub(baseline)
    );
    assert!(
        peak.saturating_sub(baseline) < 32 * 1024,
        "dashboard must not retain full telemetry ledger"
    );
}

#[test]
fn review_status_first_reply_is_independent_of_requested_page_ids() {
    let temp = tempfile::tempdir().expect("valid status parity fixture");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("valid status parity fixture");
    home.initialize().expect("valid status parity fixture");
    let created = call(&home, "group_create", json!({"title":"status dependency review"}));
    let id = created["group"]["group_id"].as_str().expect("valid status parity fixture");
    call(&home, "group_stop", json!({"group_id":id,"by":"user"}));
    call(&home, "actor_add", json!({"group_id":id,"actor_id":"peer","by":"user"}));
    let path = GroupStore::new(home.clone()).expect("valid status parity fixture").ledger_path(id).expect("valid status parity fixture");
    let mut source = Event::new("chat.message", id);
    source.by = "user".into();
    source.data = json!({"to":["peer"],"text":"request","message_mode":"request_reply"}).as_object().expect("valid status parity fixture").clone();
    let mut reply = Event::new("chat.message", id);
    reply.by = "peer".into();
    reply.data = json!({"reply_to":source.id,"to":["user"],"text":"first reply","message_mode":"send"}).as_object().expect("valid status parity fixture").clone();
    let mut cancel = Event::new("chat.reply_request.cancelled", id);
    cancel.data = json!({"source_event_id":source.id}).as_object().expect("valid status parity fixture").clone();
    for event in [&source, &reply, &cancel, &reply] {
        cccc_core::ledger::append(&path, event).expect("valid status parity fixture");
    }
    let source_only = call(&home, "ledger_statuses", json!({"group_id":id,"event_ids":[source.id]}));
    let both = call(&home, "ledger_statuses", json!({"group_id":id,"event_ids":[source.id,reply.id]}));
    let source_status = &source_only["statuses"][&source.id];
    assert_eq!(source_status["obligation_status"]["peer"]["replied"], true);
    assert_eq!(source_status["obligation_status"]["peer"]["cancelled"], false);
    assert_eq!(source_status, &both["statuses"][&source.id],
        "requesting another page record must not change which reply preceded cancellation");
}
