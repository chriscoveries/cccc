use super::super::block_on_managed;
use super::*;
use serde_json::json;
use std::sync::atomic::Ordering;
use std::time::Duration;

#[test]
fn headless_delivery_does_not_start_a_viewer_and_waits_between_turns() {
    let temp = tempfile::tempdir().expect("provider root");
    let home = HomeLayout::from_path(temp.path().join("cccc")).expect("home");
    let store = cccc_core::GroupStore::new(home.clone()).expect("store");
    let group = store.create("headless delivery", "").expect("group");
    let mut actor = Actor::new("kilo");
    actor.runtime = ActorRuntime::Kilo;
    actor
        .env
        .insert("CCCC_OPENCODE_HEADLESS".into(), "1".into());
    store
        .mutate(&group.group_id, |doc| {
            doc.actors.push(actor.clone());
            Ok(())
        })
        .expect("actor");
    let group = store.load(&group.group_id).expect("group");
    let root = temp.path().to_owned();
    let app = run_managed_launch(async move { AnalystSession::acp_for_headless_test(&root).await })
        .expect("ACP fixture");
    attach_managed(
        &home,
        &group,
        &actor,
        (group.group_id.clone(), actor.id.clone()),
        temp.path().into(),
        app,
    )
    .expect("attach");
    let item = lookup(&(group.group_id.clone(), actor.id.clone())).expect("session");
    assert!(
        cccc_runtime::status(&group.group_id, &actor.id).is_err(),
        "headless startup must not create a TUI"
    );
    let event = |text: &str| {
        let mut event = Event::new("chat.message", &group.group_id);
        event.by = "user".into();
        event.data = json!({"text":text,"to":["kilo"]})
            .as_object()
            .expect("object")
            .clone();
        event
    };
    let first = event("FIRST");
    let second = event("SECOND");
    let cancelled = AtomicBool::new(false);
    assert!(submit_batch(
        &home,
        &group,
        &actor,
        std::slice::from_ref(&first),
        &cancelled
    ));
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while item.status.lock().expect("status").status != "working" {
        assert!(
            std::time::Instant::now() < deadline,
            "ACP admission must report working"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    // Simulate ledger acknowledgement retry: same ID, same provider prompt.
    assert!(submit_batch(
        &home,
        &group,
        &actor,
        std::slice::from_ref(&first),
        &cancelled
    ));
    assert!(!submit_batch(
        &home,
        &group,
        &actor,
        std::slice::from_ref(&second),
        &cancelled
    ));
    let prompts = || {
        std::fs::read_to_string(temp.path().join("prompts.jsonl"))
            .expect("prompts")
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        prompts().len(),
        1,
        "busy turn must queue, duplicate receipt must dedupe"
    );
    let first_viewer = super::super::lazy_viewer::acquire(&group.group_id, &actor.id)
        .expect("viewer")
        .expect("lease");
    let second_viewer = super::super::lazy_viewer::acquire(&group.group_id, &actor.id)
        .expect("second viewer")
        .expect("lease");
    assert!(item.has_terminal());
    drop(first_viewer);
    assert!(
        item.has_terminal(),
        "one remaining viewer keeps the TUI alive"
    );
    drop(second_viewer);
    assert!(
        !item.has_terminal(),
        "last viewer must release the TUI runtime"
    );
    assert!(
        item.running(),
        "closing a viewer must preserve the ACP provider"
    );
    assert!(
        super::super::lazy_viewer::interrupt(&group.group_id, &actor.id, b"\x03")
            .expect("ACP cancel")
    );
    assert!(
        item.running(),
        "web interrupt cancels a turn, not the ACP server"
    );
    std::fs::write(temp.path().join("finish-first"), "").expect("finish first");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while item.status.lock().expect("status").status != "idle" {
        assert!(
            std::time::Instant::now() < deadline,
            "ACP completion must restore idle"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(submit_batch(
        &home,
        &group,
        &actor,
        std::slice::from_ref(&second),
        &cancelled
    ));
    let recorded = prompts();
    assert_eq!(recorded.len(), 2);
    assert!(recorded[0].contains("FIRST"));
    assert!(recorded[1].contains("SECOND"));
    assert!(
        !recorded[1].contains("You are"),
        "startup context is consumed once"
    );
    assert!(!item.has_terminal());
    assert_eq!(block_on_managed(item.managed.delivery_receipt_count()), 2);
    for source in [&first, &second] {
        crate::ops::runtime_delivery::append_state(
            &home,
            &group.group_id,
            &actor.id,
            &actor.created_at,
            &source.id,
            "managed_session",
            crate::ops::runtime_delivery::DeliveryOutcome::Accepted,
        )
        .expect("durable ack");
        super::super::lazy_viewer::delivery_accepted(&group.group_id, &actor.id, &source.id);
        assert!(
            matches!(crate::ops::runtime_delivery::claim(&home, &group, &actor, &source.id, "managed_session", false)
            .expect("claim fence"), crate::ops::runtime_delivery::ClaimResult::Terminal(state) if state == "accepted")
        );
    }
    assert_eq!(block_on_managed(item.managed.delivery_receipt_count()), 0);
    // An explicit rejection has no admission receipt and can safely retry the same ID.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while item.status.lock().expect("status").status != "idle" {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let retry = event("RETRY");
    assert!(!submit_batch(
        &home,
        &group,
        &actor,
        std::slice::from_ref(&retry),
        &cancelled
    ));
    assert_eq!(block_on_managed(item.managed.delivery_receipt_count()), 0);
    let failure: serde_json::Value = std::fs::read_to_string(
        store
            .state_dir(&group.group_id)
            .expect("state")
            .join("headless/events.jsonl"),
    )
    .expect("events")
    .lines()
    .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("event"))
    .find(|event| event["type"] == "headless.turn.failed")
    .expect("visible error");
    assert_eq!(failure["data"]["pending_event_id"], retry.id);
    assert_eq!(failure["data"]["retryable"], true);
    assert!(
        failure["data"]["error"]["message"]
            .as_str()
            .expect("message")
            .contains(&retry.id)
    );
    assert!(submit_batch(
        &home,
        &group,
        &actor,
        std::slice::from_ref(&retry),
        &cancelled
    ));
    assert_eq!(prompts().len(), 3);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while item.status.lock().expect("status").status != "idle" {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let uncertain = event("AMBIGUOUS - DO NOT REPLAY");
    let job = crate::ops::actor_delivery::DeliveryJob {
        home: home.clone(),
        group: group.clone(),
        actor: actor.clone(),
        event: uncertain.clone(),
    };
    assert!(
        crate::ops::actor_delivery_worker::process_batch(
            std::slice::from_ref(&job),
            &mut String::new(),
            &cancelled
        ),
        "uncertain source must settle without blocking the queue"
    );
    assert_eq!(
        crate::ops::runtime_delivery::latest_state(
            &home,
            &group.group_id,
            &actor.id,
            &uncertain.id
        )
        .expect("outcome")
        .expect("record")
        .0,
        "ambiguous"
    );
    assert!(
        matches!(crate::ops::runtime_delivery::claim(&home, &group, &actor, &uncertain.id, "managed_session", false)
        .expect("claim fence"), crate::ops::runtime_delivery::ClaimResult::Terminal(state) if state == "ambiguous")
    );
    assert_eq!(
        prompts().len(),
        3,
        "uncertain admission must never be replayed"
    );
    let retained = block_on_managed(item.managed.unresolved_delivery(&uncertain.id))
        .expect("uncertain receipt");
    assert!(
        retained.len() <= 4099,
        "large provider diagnostics must not inflate the bounded receipt cache"
    );

    assert!(!item.stopped.load(Ordering::Acquire));
    stop(&group.group_id, &actor.id).expect("stop");
}
