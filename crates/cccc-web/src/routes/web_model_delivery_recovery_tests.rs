use super::*;
use axum::{
    Json, Router,
    response::Html,
    routing::{get, post},
};
use cccc_contracts::{Actor, ActorRuntime, DaemonRequest, Event};
use cccc_core::{GroupStore, HomeLayout, ledger};
use std::sync::{Arc, Mutex};

/// On Unix, a child spawned by a concurrently running test can keep an
/// inherited copy of the lock's descriptor until it execs, and the flock
/// with it, so a released lock can stay busy until that copy closes.
fn reclaim<T>(mut attempt: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let claimed = attempt();
        if claimed.is_some() || std::time::Instant::now() >= deadline {
            return claimed;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn browser_work_and_a_switch_exclude_each_other_without_waiting() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    let path = cccc_core::web_model_connectors::browser_work_lock(&home, "group", "actor");
    let lock = || cccc_core::fs::try_exclusive_lock(&path).expect("lock");

    let switch = lock().expect("free");
    assert!(
        browser_work_in(&home, "group", "actor")
            .expect("work")
            .is_none()
    );
    drop(switch);

    let work = reclaim(|| browser_work_in(&home, "group", "actor").expect("work"))
        .expect("released switch lock");
    assert!(lock().is_none());
    assert!(
        browser_work_in(&home, "group", "actor")
            .expect("work")
            .is_none()
    );
    drop(work);
    assert!(reclaim(lock).is_some());
}

#[test]
fn rejected_session_guards_do_not_release_the_active_owner() {
    for registry in [&WORKERS, &IN_FLIGHT] {
        let key = "guard-contention-regression".to_owned();
        let owner = SessionGuard::acquire(registry, key.clone()).expect("first owner");
        for _ in 0..4 {
            assert!(
                SessionGuard::acquire(registry, key.clone()).is_none(),
                "a rejected contender must leave the active owner registered"
            );
        }
        let other = SessionGuard::acquire(registry, "other-actor-guard-regression".into())
            .expect("another Actor remains independent");
        drop(other);
        assert!(SessionGuard::acquire(registry, key.clone()).is_none());
        drop(owner);
        assert!(SessionGuard::acquire(registry, key).is_some());
    }
}

#[tokio::test]
async fn settled_browser_turns_are_reviewed_without_resubmitting_their_drafts() {
    if crate::system_browser_path().is_none() {
        return;
    }
    let _chrome_guard = crate::browser_surface::chrome_test_guard().await;
    let temp = tempfile::tempdir().expect("tempdir");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    home.initialize().expect("initialize");
    let store = GroupStore::new(home.clone()).expect("store");
    let mut group = store.create("restart recovery", "").expect("group");
    group.running = true;
    let mut actor = Actor::new("browser-test");
    actor.runtime = ActorRuntime::WebModel;
    actor.normalize_runtime_constraints();
    actor.generation = "fixture-generation".into();
    actor
        .env
        .insert("CCCC_WEB_MODEL_DELIVERY_MODE".into(), "browser".into());
    group.actors.push(actor);
    group.extra.insert(
        super::super::web_model_browser::TARGETS_KEY.into(),
        json!({}),
    );
    store.save(&group).expect("save");
    let ledger_path = store.ledger_path(&group.group_id).expect("ledger");
    let message = |text: &str| {
        let mut event = Event::new("chat.message", &group.group_id);
        event.by = "user".into();
        event.data.insert("message_mode".into(), json!("send"));
        event.data.insert("to".into(), json!(["browser-test"]));
        event.data.insert("text".into(), json!(text));
        ledger::append(&ledger_path, &event).expect("message");
        event
    };
    let first = message("claimed before a target fence");
    let daemon = crate::test_daemon::TestDaemon::start(&home).await;
    let client = cccc_client::DaemonClient::new(home.clone());
    let call = |op: &'static str, extra: Value| {
        let client = client.clone();
        let group_id = group.group_id.clone();
        async move {
            let mut args =
                json!({"group_id":group_id,"actor_id":"browser-test","by":"browser-test"});
            args.as_object_mut()
                .expect("args")
                .extend(extra.as_object().cloned().expect("extra"));
            let response = client
                .call(&DaemonRequest {
                    v: 1,
                    op: op.into(),
                    args: args.as_object().expect("args").clone(),
                })
                .await
                .expect("daemon request");
            assert!(response.ok, "{:?}", response.error);
            response.result
        }
    };
    let shutdown = tokio::sync::broadcast::channel(1).0;
    let (_, _, surfaces, state) = crate::app_with_shutdown(
        home.clone(),
        shutdown.clone(),
        crate::WebMode::Normal,
        None,
        crate::LiveBinding {
            host: "127.0.0.1".into(),
            port: 0,
        },
        "restart-recovery-test".into(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listen");
    let url = format!("http://{}/", listener.local_addr().expect("address"));
    let server = tokio::spawn(async move {
        axum::serve(listener, Router::new().route("/", get(|| async {
            Html(r#"<form onsubmit="event.preventDefault();window.attempts++;const t=document.querySelector('textarea');const a=document.createElement('article');a.dataset.messageAuthorRole='user';a.textContent=t.value;document.body.append(a);t.value=''"><textarea id="prompt-textarea" style="width:500px;height:120px"></textarea><button type="submit">Send</button></form><script>window.attempts=0</script>"#)
        }))).await.expect("serve");
    });
    let session_key = key(&group.group_id, "browser-test");
    surfaces
        .open_shared_actor_fixture(
            &session_key,
            &temp.path().join("profile"),
            &url,
            &crate::browser_surface::actor_identity(&group.actors[0]),
        )
        .await
        .expect("browser");
    let page = surfaces
        .sessions
        .lock()
        .await
        .get(&session_key)
        .expect("session")
        .page
        .clone();
    super::login_tests::pair_fixture(&state, &group.group_id, &url);
    update_target(
        &state,
        &group.group_id,
        "browser-test",
        json!({"kind":"existing_chat","url":url,"last_delivery_id":"previous",
               "last_delivery_turn_id":"previous-turn","last_delivery_status":"bound"}),
    )
    .expect("target");

    let wait = call(
        "runtime_wait_next_turn",
        json!({"transport":"web_model_browser"}),
    )
    .await;
    let turn = &wait["turn"];
    assert_eq!(turn["event_ids"], json!([first.id]));
    call(
        "web_model_browser_delivery_record",
        json!({"turn_id":turn["turn_id"],"event_ids":turn["event_ids"],"delivery_id":"interrupted",
               "browser_delivery":{"state":"ambiguous","detail":"daemon restarted"}}),
    )
    .await;
    // Opening a shared actor page starts navigation before its document is ready.
    let fixture_ready = format!(
        "location.href === {} && \
         ['interactive', 'complete'].includes(document.readyState) && \
         !!document.querySelector('textarea#prompt-textarea') && window.attempts === 0",
        json!(url)
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !page
            .evaluate(fixture_ready.as_str())
            .await
            .is_ok_and(|result| result.into_value::<bool>().unwrap_or(false))
        {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("fixture document and composer ready");
    let outcome = deliver_pending(&state, &group.group_id, "browser-test")
        .await
        .expect("recover");
    let outcome_name = match outcome {
        DeliveryOutcome::Ambiguous => "Ambiguous",
        DeliveryOutcome::Deferred(_) => "Deferred",
        DeliveryOutcome::Idle => "Idle",
        DeliveryOutcome::Stopped => "Stopped",
        DeliveryOutcome::Submitted => "Submitted",
    };
    assert_eq!(
        outcome_name, "Ambiguous",
        "interrupted browser turn recovery"
    );
    let target = load_target(&state, &group.group_id, "browser-test").expect("target");
    let expected_id = browser_delivery_id("browser-test", turn["turn_id"].as_str().expect("turn"));
    assert_eq!(target["last_delivery_status"], "submission_ambiguous");
    assert_eq!(target["last_delivery_id"], expected_id);
    assert_eq!(target["last_delivery_turn_id"], turn["turn_id"]);
    assert_eq!(target["last_delivery_event_ids"], turn["event_ids"]);
    assert_eq!(
        call("headless_status", json!({})).await["state"]["active_turn_id"],
        ""
    );
    resume_after_review(&state, &group.group_id, "browser-test", &expected_id)
        .await
        .expect("user reviews first turn");

    // An older daemon could record failed without releasing the active turn.
    // Its staged draft must survive the same conservative recovery.
    let second = message("legacy failed turn");
    let wait = call(
        "runtime_wait_next_turn",
        json!({"transport":"web_model_browser"}),
    )
    .await;
    let turn = &wait["turn"];
    assert_eq!(turn["event_ids"], json!([second.id]));
    let mut failure = Event::new("runtime.delivery", &group.group_id);
    failure.by = "system".into();
    failure.data = json!({"actor_id":"browser-test","source_event_id":second.id,
        "state":"failed","transport":"web_model_browser"})
    .as_object()
    .expect("delivery")
    .clone();
    ledger::append(&ledger_path, &failure).expect("legacy failure");
    update_target(
        &state,
        &group.group_id,
        "browser-test",
        json!({"last_delivery_status":"deferred","last_delivery_turn_id":turn["turn_id"]}),
    )
    .expect("staged target");
    page.evaluate("document.querySelector('textarea').value='staged draft'")
        .await
        .expect("stage draft");
    assert!(matches!(
        deliver_pending(&state, &group.group_id, "browser-test")
            .await
            .expect("recover legacy"),
        DeliveryOutcome::Ambiguous
    ));
    let target = load_target(&state, &group.group_id, "browser-test").expect("target");
    assert_eq!(target["last_delivery_status"], "submission_ambiguous");
    assert_eq!(target["last_delivery_turn_id"], turn["turn_id"]);
    assert_eq!(
        call("headless_status", json!({})).await["state"]["active_turn_id"],
        ""
    );
    assert_eq!(
        page.evaluate("document.querySelector('textarea').value")
            .await
            .expect("draft")
            .into_value::<String>()
            .expect("text"),
        "staged draft"
    );
    assert_eq!(
        page.evaluate("window.attempts")
            .await
            .expect("attempts")
            .into_value::<u32>()
            .expect("number"),
        0
    );
    let states =
        cccc_core::inbox::delivery_states(&home, &group, &["browser-test".into()], &second.id)
            .expect("states");
    assert_eq!(states["browser-test"].0, "ambiguous");

    // A still-claimed deferred turn is different: after the user reviews the
    // old uncertain batch, its own exact staged draft must be submitted once.
    page.evaluate("document.querySelector('textarea').value=''")
        .await
        .expect("clear reviewed draft");
    let previous_id = target["last_delivery_id"].as_str().expect("review id");
    resume_after_review(&state, &group.group_id, "browser-test", previous_id)
        .await
        .expect("user reviews legacy turn");
    let third = message("continue the same deferred turn");
    let wait = call(
        "runtime_wait_next_turn",
        json!({"transport":"web_model_browser"}),
    )
    .await;
    let turn = &wait["turn"];
    assert_eq!(turn["event_ids"], json!([third.id]));
    let turn_id = turn["turn_id"].as_str().expect("turn");
    let delivery_id = browser_delivery_id("browser-test", turn_id);
    let target = load_target(&state, &group.group_id, "browser-test").expect("target");
    let (prompt, _) =
        build_browser_prompt(turn, &target, &url, "browser-test", &delivery_id, &third.id)
            .expect("prompt");
    update_target(
        &state,
        &group.group_id,
        "browser-test",
        json!({"last_delivery_status":"deferred","last_delivery_id":delivery_id,
               "last_delivery_turn_id":turn_id,"last_delivery_event_ids":turn["event_ids"]}),
    )
    .expect("staged target");
    page.evaluate(format!(
        "document.querySelector('textarea').value={}",
        json!(prompt)
    ))
    .await
    .expect("stage deferred batch");
    assert!(matches!(
        deliver_pending(&state, &group.group_id, "browser-test")
            .await
            .expect("resume claimed batch"),
        DeliveryOutcome::Submitted
    ));
    assert_eq!(
        page.evaluate("window.attempts")
            .await
            .expect("attempts")
            .into_value::<u32>()
            .expect("number"),
        1
    );
    let states =
        cccc_core::inbox::delivery_states(&home, &group, &["browser-test".into()], &third.id)
            .expect("states");
    assert_eq!(states["browser-test"].0, "accepted");
    assert_eq!(
        call("headless_status", json!({})).await["state"]["active_turn_id"],
        ""
    );
    assert!(matches!(
        deliver_pending(&state, &group.group_id, "browser-test")
            .await
            .expect("poll after completed turn"),
        DeliveryOutcome::Idle
    ));
    assert_eq!(
        page.evaluate("window.attempts")
            .await
            .expect("attempts")
            .into_value::<u32>()
            .expect("number"),
        1,
        "the completed batch must not be submitted again"
    );

    let fourth = message("send once after the draft clears");
    // The second composer inspection in this attempt runs after the daemon
    // claim. Model a human beginning a draft at that point.
    page.evaluate(
        r#"() => {
        const box = document.querySelector('textarea');
        const original = box.setAttribute.bind(box);
        let probes = 0;
        box.setAttribute = (name, value) => {
            if (name === 'data-cccc-web-model-composer' && ++probes === 2) {
                box.value = 'Unsent draft';
                delete box.setAttribute;
            }
            original(name, value);
        };
    }"#,
    )
    .await
    .expect("draft during claim");
    assert!(matches!(
        deliver_pending(&state, &group.group_id, "browser-test")
            .await
            .expect("preflight"),
        DeliveryOutcome::Idle
    ));
    let held = load_target(&state, &group.group_id, "browser-test").expect("held claim");
    assert_eq!(held["last_delivery_status"], "preflight_deferred");
    assert_eq!(held["last_delivery_event_ids"], json!([fourth.id]));
    assert_eq!(
        held["last_submission_evidence"]["submission_evidence"],
        "send_not_started"
    );
    for _ in 0..2 {
        assert!(matches!(
            deliver_pending(&state, &group.group_id, "browser-test")
                .await
                .expect("draft held"),
            DeliveryOutcome::Idle
        ));
        assert_eq!(
            load_target(&state, &group.group_id, "browser-test").expect("same claim"),
            held
        );
        assert_eq!(
            call("headless_status", json!({})).await["state"]["active_turn_id"],
            held["last_delivery_turn_id"]
        );
    }
    assert_eq!(
        page.evaluate("document.querySelector('textarea').value")
            .await
            .expect("draft")
            .into_value::<String>()
            .expect("text"),
        "Unsent draft"
    );
    page.evaluate(r#"() => {
        document.querySelector('textarea').value = '';
        const banner = document.createElement('div');
        banner.id = 'conversation-limit';
        banner.textContent = "You've reached the maximum length for this conversation, but you can keep talking by starting a new chat.";
        document.body.append(banner);
    }"#).await.expect("full-chat banner");
    assert!(matches!(
        deliver_pending(&state, &group.group_id, "browser-test")
            .await
            .expect("full chat"),
        DeliveryOutcome::Idle
    ));
    assert_eq!(
        load_target(&state, &group.group_id, "browser-test").expect("same claim"),
        held
    );
    page.evaluate("document.querySelector('#conversation-limit').remove()")
        .await
        .expect("recover page");
    assert!(matches!(
        deliver_pending(&state, &group.group_id, "browser-test")
            .await
            .expect("resume exact claim"),
        DeliveryOutcome::Submitted
    ));
    let delivered = load_target(&state, &group.group_id, "browser-test").expect("delivered");
    assert_eq!(
        delivered["last_delivery_turn_id"],
        held["last_delivery_turn_id"]
    );
    assert_eq!(delivered["last_delivery_id"], held["last_delivery_id"]);
    assert_eq!(
        page.evaluate("window.attempts")
            .await
            .expect("attempts")
            .into_value::<u32>()
            .expect("number"),
        2,
        "one send for each resumed batch"
    );
    assert!(matches!(
        deliver_pending(&state, &group.group_id, "browser-test")
            .await
            .expect("queue drained"),
        DeliveryOutcome::Idle
    ));

    // A previous delivery can be settled while its staged draft still occupies the composer.
    let old = message("leave the staged batch in the composer");
    page.evaluate("document.querySelector('button').disabled = true")
        .await
        .expect("disable submit while staging");
    assert!(matches!(
        deliver_pending(&state, &group.group_id, "browser-test")
            .await
            .expect("stage without dispatch"),
        DeliveryOutcome::Deferred(_)
    ));
    let deferred = load_target(&state, &group.group_id, "browser-test").expect("staged attempt");
    assert_eq!(deferred["last_delivery_event_ids"], json!([old.id]));
    let delivery_id = required(&deferred, "last_delivery_id").expect("delivery ID");
    let turn_id = required(&deferred, "last_delivery_turn_id").expect("turn ID");
    let draft = || async {
        page.evaluate("document.querySelector('textarea').value")
            .await
            .expect("read composer")
            .into_value::<String>()
            .expect("composer text")
    };
    let staged = draft().await;
    assert!(staged.contains(&old.id));
    assert_eq!(
        deferred["staged_draft"]["sha256"].as_str(),
        Some(format!("{:x}", Sha256::digest(staged.as_bytes())).as_str())
    );

    // Settle through the worker's retry-budget completion path without waiting for its timers.
    let evidence = json!({
        "submitted": false,
        "submission_evidence": "deferred_retry_budget_exhausted",
        "send_not_dispatched": true
    });
    assert!(matches!(
        complete_ambiguous_attempt(
            &state,
            &group.group_id,
            "browser-test",
            DeliveryAttempt {
                turn_id,
                event_ids: deferred["last_delivery_event_ids"].clone(),
                delivery_id,
            },
            evidence,
            "deferred retry budget exhausted",
        )
        .await
        .expect("settle the unsent attempt"),
        DeliveryOutcome::Ambiguous
    ));
    let old_receipts = || {
        std::fs::read_to_string(&ledger_path)
            .expect("ledger")
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("ledger event"))
            .filter(|event| {
                event["data"]["source_event_id"] == old.id
                    || event["data"]["turn_id"] == turn_id
                    || event["data"]["delivery_id"] == delivery_id
            })
            .collect::<Vec<_>>()
    };
    let settled_receipts = old_receipts();
    assert!(settled_receipts.iter().any(|event| {
        event["kind"] == "runtime.delivery" && event["data"]["state"] == "ambiguous"
    }));
    assert_eq!(
        call("headless_status", json!({})).await["state"]["active_turn_id"],
        ""
    );

    // Edit only the text, leaving the managed target and the stored proof intact.
    let edited = staged.replacen("leave the staged batch", "Leave the staged batch", 1);
    assert_ne!(edited, staged);
    page.evaluate(format!(
        r#"(() => {{
            document.querySelector('button').disabled = false;
            const box = document.querySelector('textarea');
            box.value = {};
            window.laterSubmissions = [];
            box.form.addEventListener('submit', () => window.laterSubmissions.push(box.value), true);
        }})()"#,
        json!(edited)
    ))
    .await
    .expect("edit the draft and track later submissions");
    let later = message("deliver only this later event");
    let later_submissions = || async {
        page.evaluate("window.laterSubmissions")
            .await
            .expect("submissions")
            .into_value::<Vec<String>>()
            .expect("submitted prompts")
    };
    assert!(matches!(
        deliver_pending(&state, &group.group_id, "browser-test")
            .await
            .expect("preserve the edit"),
        DeliveryOutcome::Ambiguous
    ));
    assert_eq!(draft().await, edited);
    assert!(later_submissions().await.is_empty());
    assert_eq!(
        call("headless_status", json!({})).await["state"]["active_turn_id"],
        ""
    );

    page.evaluate(format!(
        "document.querySelector('textarea').value={}",
        json!(staged)
    ))
    .await
    .expect("restore the exact staged text");
    assert!(matches!(
        deliver_pending(&state, &group.group_id, "browser-test")
            .await
            .expect("automatically clear the owned draft"),
        DeliveryOutcome::Idle
    ));
    assert_eq!(draft().await, "");
    assert!(later_submissions().await.is_empty());
    assert!(matches!(
        deliver_pending(&state, &group.group_id, "browser-test")
            .await
            .expect("deliver the later event"),
        DeliveryOutcome::Submitted
    ));
    assert!(matches!(
        deliver_pending(&state, &group.group_id, "browser-test")
            .await
            .expect("poll the drained queue"),
        DeliveryOutcome::Idle
    ));
    let submitted = later_submissions().await;
    assert_eq!(submitted.len(), 1);
    assert!(submitted[0].contains(&later.id));
    assert!(!submitted[0].contains(&old.id));
    let later_states =
        cccc_core::inbox::delivery_states(&home, &group, &["browser-test".into()], &later.id)
            .expect("later delivery state");
    assert_eq!(later_states["browser-test"].0, "accepted");
    assert_eq!(old_receipts(), settled_receipts);

    surfaces.close(&session_key).await.expect("close browser");
    let _ = shutdown.send(());
    server.abort();
    daemon.stop().await;
}

#[tokio::test]
async fn verified_legacy_draft_recovers_through_rebuilt_prompt_once() {
    if crate::system_browser_path().is_none() {
        return;
    }
    let _chrome_guard = crate::browser_surface::chrome_test_guard().await;
    let temp = tempfile::tempdir().expect("fixture");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    home.initialize().expect("initialize");
    let store = GroupStore::new(home.clone()).expect("store");
    let mut group = store.create("legacy recovery", "").expect("group");
    let mut actor = Actor::new("browser-test");
    actor.runtime = ActorRuntime::WebModel;
    actor.normalize_runtime_constraints();
    group.actors.push(actor);
    group.extra.insert(
        super::super::web_model_browser::TARGETS_KEY.into(),
        json!({}),
    );
    // Keep the fixture Group stopped: only explicit offline recovery is exercised.
    assert!(!group.running);
    store.save(&group).expect("save");
    let mut event = Event::new("chat.message", &group.group_id);
    event.by = "user".into();
    event.data.insert("to".into(), json!(["browser-test"]));
    event
        .data
        .insert("text".into(), json!("Recover this exact old draft."));
    let ledger_path = store.ledger_path(&group.group_id).expect("ledger path");
    ledger::append(&ledger_path, &event).expect("seed message");
    let daemon_home = home.clone();
    let daemon = tokio::spawn(async move { cccc_daemon::run(daemon_home).await });
    let client = cccc_client::DaemonClient::new(home.clone());
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if client
                .call(&DaemonRequest {
                    v: 1,
                    op: "ping".into(),
                    args: Default::default(),
                })
                .await
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("daemon ready");
    let recorded = client.call(&DaemonRequest {v:1, op:"web_model_browser_delivery_record".into(), args:json!({
        "group_id":group.group_id,"actor_id":"browser-test","turn_id":"old-turn",
        "event_ids":[event.id],"delivery_id":"wmd_original","browser_delivery":{"state":"submitted"}
    }).as_object().expect("args").clone()}).await.expect("seed legacy handoff");
    assert!(recorded.ok, "{recorded:?}");

    let shutdown = tokio::sync::broadcast::channel(1).0;
    let (_, _, surfaces, state) = crate::app_with_shutdown(
        home.clone(),
        shutdown.clone(),
        crate::WebMode::Normal,
        None,
        crate::LiveBinding {
            host: "127.0.0.1".into(),
            port: 0,
        },
        "legacy-test".into(),
    );
    let submitted = Arc::new(Mutex::new(Vec::<String>::new()));
    let submitted_handler = submitted.clone();
    let old = "[user -> browser-test] Recover this exact old draft.";
    let html = format!(
        r#"<main><form onsubmit="event.preventDefault(); const t=document.querySelector('textarea'); const text=t.value; const a=document.createElement('article'); a.dataset.messageAuthorRole='user'; a.textContent=text; document.querySelector('main').append(a); t.value=''; history.replaceState(null,'','/c/recovered-legacy'); fetch('/submitted',{{method:'POST',headers:{{'Content-Type':'application/json'}},body:JSON.stringify(text)}});"><textarea id='prompt-textarea' style='width:500px;height:150px'></textarea><button type='submit'>Send</button></form></main><script>document.querySelector('textarea').value={};</script>"#,
        serde_json::to_string(old).expect("old text")
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listen");
    let url = format!("http://{}/", listener.local_addr().expect("address"));
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route(
                    "/",
                    get(move || {
                        let html = html.clone();
                        async move { Html(html) }
                    }),
                )
                .route(
                    "/submitted",
                    post(move |Json(text): Json<String>| {
                        let submitted = submitted_handler.clone();
                        async move {
                            submitted.lock().expect("submissions").push(text);
                            Json(json!({"ok":true}))
                        }
                    }),
                ),
        )
        .await
        .expect("serve");
    });
    let session_key = key(&group.group_id, "browser-test");
    surfaces
        .open(&session_key, &temp.path().join("profile"), &url, 800, 600)
        .await
        .expect("browser");
    let target = json!({"kind":"new_chat","url":url,"last_delivery_status":"pending_new_chat_bind",
        "last_delivery_id":"wmd_original","last_delivery_turn_id":"old-turn","last_delivery_event_ids":[event.id]});
    update_target(&state, &group.group_id, "browser-test", target.clone()).expect("target");
    assert!(is_legacy_pending_delivery(&target));
    let original_ledger = std::fs::read(&ledger_path).expect("before");
    let result = recover_legacy_pending_delivery(
        &state,
        &group.group_id,
        "browser-test",
        &session_key,
        &target,
    )
    .await;
    let final_target = load_target(&state, &group.group_id, "browser-test").expect("final target");
    let messages = submitted.lock().expect("submissions").clone();
    let final_ledger = std::fs::read(&ledger_path).expect("after");
    surfaces.close(&session_key).await.expect("close browser");
    let _ = shutdown.send(());
    server.abort();
    client
        .call(&DaemonRequest {
            v: 1,
            op: "shutdown".into(),
            args: Default::default(),
        })
        .await
        .expect("stop daemon");
    daemon.await.expect("daemon task").expect("daemon stopped");

    assert!(
        matches!(result, Ok(DeliveryOutcome::Submitted)),
        "recovery did not submit: {final_target}"
    );
    assert_eq!(messages.len(), 1, "must submit only the recovered batch");
    assert!(messages[0].contains("Recover this exact old draft."));
    assert!(messages[0].contains("Session bootstrap"));
    assert_ne!(messages[0], old, "recovery rebuilds the prompt");
    // Legacy recovery records evidence but cannot grant connector authority.
    assert_eq!(final_target["kind"], "none");
    assert_eq!(
        cccc_core::integration_state::group_get(
            &store,
            &group.group_id,
            super::super::web_model_browser::TARGETS_KEY
        )
        .expect("valid test fixture")["browser-test"]["kind"],
        "existing_chat"
    );
    assert_eq!(final_target["last_delivery_status"], "submitted");
    assert!(
        final_target["last_delivery_id"]
            .as_str()
            .expect("id")
            .starts_with("webdelivery:")
    );
    assert!(!is_legacy_pending_delivery(&final_target));
    assert_eq!(
        original_ledger, final_ledger,
        "recovery must not replay ledger delivery/completion or consume Mail"
    );
}

#[tokio::test]
async fn contended_worker_recovers_manual_send_and_only_delivers_later_work() {
    use chromiumoxide::cdp::browser_protocol::fetch::{
        EnableParams, EventRequestPaused, FulfillRequestParams, HeaderEntry,
    };
    use futures_util::StreamExt;

    if crate::system_browser_path().is_none() {
        return;
    }
    let _chrome_guard = crate::browser_surface::chrome_test_guard().await;
    let temp = tempfile::tempdir().expect("fixture");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    home.initialize().expect("initialize");
    let store = GroupStore::new(home.clone()).expect("store");
    let mut group = store.create("delivery recovery", "").expect("group");
    group.running = true;
    let mut actor = Actor::new("browser-test");
    actor.runtime = ActorRuntime::WebModel;
    actor.normalize_runtime_constraints();
    actor.generation = "fixture-generation".into();
    actor
        .env
        .insert("CCCC_WEB_MODEL_DELIVERY_MODE".into(), "browser".into());
    group.actors.push(actor);
    group.extra.insert(
        super::super::web_model_browser::TARGETS_KEY.into(),
        json!({}),
    );
    store.save(&group).expect("save");
    let ledger_path = store.ledger_path(&group.group_id).expect("ledger");
    let mut first = Event::new("chat.message", &group.group_id);
    first.by = "user".into();
    first.data.insert("message_mode".into(), json!("send"));
    first.data.insert("to".into(), json!(["browser-test"]));
    first
        .data
        .insert("text".into(), json!("First message, manually sent later."));
    ledger::append(&ledger_path, &first).expect("first");
    let daemon_home = home.clone();
    let daemon = tokio::spawn(async move { cccc_daemon::run(daemon_home).await });
    let client = cccc_client::DaemonClient::new(home.clone());
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if client
                .call(&DaemonRequest {
                    v: 1,
                    op: "ping".into(),
                    args: Default::default(),
                })
                .await
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("daemon ready");
    let shutdown = tokio::sync::broadcast::channel(1).0;
    let (_, _, surfaces, state) = crate::app_with_shutdown(
        home,
        shutdown.clone(),
        crate::WebMode::Normal,
        None,
        crate::LiveBinding {
            host: "127.0.0.1".into(),
            port: 0,
        },
        "delivery-recovery-test".into(),
    );
    const HTML: &str = r#"<main><article data-message-author-role="user">Older message</article><form onsubmit="event.preventDefault();window.attempts++;if(window.accept)send();else restoreHistory()"><textarea id="prompt-textarea" style="width:500px;height:120px"></textarea><button type="submit">Send</button></form></main><script>
    window.attempts=0;window.accept=false;window.sent=[];
    function restoreHistory(){const a=document.createElement('article');a.dataset.messageAuthorRole='user';a.textContent='Another older message returning to the DOM';document.querySelector('main').prepend(a);}
    function send(){const t=document.querySelector('textarea');window.sent.push(t.value);const a=document.createElement('article');a.dataset.messageAuthorRole='user';a.textContent=t.value;document.querySelector('main').append(a);t.value='';}
    </script>"#;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listen");
    let entry_url = format!("http://{}/", listener.local_addr().expect("addr"));
    let url = "https://chatgpt.com/c/recovery-fixture".to_owned();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route("/", get(|| async { Html(HTML) })),
        )
        .await
        .expect("serve");
    });
    let session_key = key(&group.group_id, "browser-test");
    surfaces
        .open(
            &session_key,
            &temp.path().join("profile"),
            &entry_url,
            800,
            600,
        )
        .await
        .expect("browser");
    let page = surfaces
        .sessions
        .lock()
        .await
        .get(&session_key)
        .expect("session")
        .page
        .clone();
    // Serve a synthetic ChatGPT origin so the same chat can change Project routes.
    // Every request is fulfilled locally, including navigation and subresources.
    let mut requests = page
        .event_listener::<EventRequestPaused>()
        .await
        .expect("intercept");
    page.execute(EnableParams::default())
        .await
        .expect("enable interception");
    let control = page.clone();
    let intercept = tokio::spawn(async move {
        while let Some(request) = requests.next().await {
            let mut response = FulfillRequestParams::new(request.request_id.clone(), 200);
            response.response_headers = Some(vec![HeaderEntry::new("Content-Type", "text/html")]);
            response.body = Some(
                base64::Engine::encode(&base64::engine::general_purpose::STANDARD, HTML).into(),
            );
            if control.execute(response).await.is_err() {
                break;
            }
        }
    });
    page.goto(&url).await.expect("offline ChatGPT page");
    super::login_tests::pair_fixture(&state, &group.group_id, &url);
    update_target(
        &state,
        &group.group_id,
        "browser-test",
        json!({"url":url,"kind":"existing_chat"}),
    )
    .expect("target");
    assert!(
        super::super::web_model_supervisor::actor_delivery_enabled(
            &state,
            &group.group_id,
            "browser-test"
        ),
        "fixture must enable delivery: {:?}",
        store.load(&group.group_id).expect("group").running
    );
    let mut held_turn = None;
    for probe in [2, 3] {
        // The second inspection runs after the claim. The third selects the
        // composer for staging after the worker's preflight succeeded.
        page.evaluate(format!(
            r#"(() => {{
            const box = document.querySelector('textarea');
            const original = box.setAttribute.bind(box);
            let probes = 0;
            box.setAttribute = (name, value) => {{
                if (name === 'data-cccc-web-model-composer' && ++probes === {probe}) {{
                    history.replaceState(null, '', '/c/unrelated-fixture');
                    delete box.setAttribute;
                }}
                original(name, value);
            }};
        }})()"#
        ))
        .await
        .expect("switch chats during inspection");
        assert!(matches!(
            deliver_pending(&state, &group.group_id, "browser-test")
                .await
                .expect("hold changed chat"),
            DeliveryOutcome::Idle
        ));
        let held = load_target(&state, &group.group_id, "browser-test").expect("held target");
        assert_eq!(held["last_delivery_status"], "preflight_deferred");
        assert_eq!(held["last_delivery_event_ids"], json!([first.id]));
        assert_eq!(
            held["last_submission_evidence"]["submission_evidence"],
            "send_not_started"
        );
        let turn = held["last_delivery_turn_id"].clone();
        assert!(!turn.as_str().expect("claimed turn ID").is_empty());
        assert_eq!(*held_turn.get_or_insert(turn.clone()), turn);
        assert_eq!(
            page.evaluate(
                "[location.pathname, document.querySelector('textarea').value, window.attempts]"
            )
            .await
            .expect("wrong chat")
            .into_value::<Value>()
            .expect("page state"),
            json!(["/c/unrelated-fixture", "", 0])
        );
        page.evaluate("history.replaceState(null,'','/g/g-p-fixture/c/recovery-fixture')")
            .await
            .expect("restore bound chat through Project alias");
    }
    let initial_state = state.clone();
    let initial_group = group.group_id.clone();
    let initial = tokio::spawn(async move {
        let outcome = deliver_pending(&initial_state, &initial_group, "browser-test")
            .await
            .expect("initial delivery");
        assert!(matches!(outcome, DeliveryOutcome::Ambiguous));
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if page
                .evaluate("window.attempts")
                .await
                .expect("attempts")
                .into_value::<u32>()
                .expect("number")
                > 0
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "first click; task finished={}, target={}",
            initial.is_finished(),
            load_target(&state, &group.group_id, "browser-test").expect("target")
        )
    });
    // Ledger/status wake-ups during the live submission must not orphan it.
    for _ in 0..8 {
        super::super::web_model_supervisor::ensure_running_actor(&state, Some(&group.group_id))
            .await;
    }
    tokio::time::timeout(std::time::Duration::from_secs(15),async {
        loop {
            if load_target(&state,&group.group_id,"browser-test").expect("target")["last_delivery_status"]=="submission_ambiguous" {break;}
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        }
    }).await.expect("first uncertainty settles");
    assert!(
        !std::fs::read_to_string(&ledger_path)
            .expect("ledger")
            .contains("interrupted after its at-most-once dispatch fence")
    );
    assert_eq!(
        page.evaluate("window.attempts")
            .await
            .expect("attempts")
            .into_value::<u32>()
            .expect("number"),
        1
    );
    initial.await.expect("initial task");
    let ambiguous = load_target(&state, &group.group_id, "browser-test").expect("target");
    assert_eq!(
        ambiguous["last_delivery_turn_id"],
        held_turn.expect("deferred turn")
    );
    assert!(
        !recover_verified_ambiguous_submission(&state, &group.group_id, "browser-test", &ambiguous)
            .await
            .expect("weak history is not recoverable evidence")
    );
    assert_eq!(
        ambiguous["last_submission_evidence"]["observed"]["user_message_count"],
        2
    );
    assert_eq!(
        ambiguous["last_submission_evidence"]["observed"]["composer_exact"],
        true
    );
    assert_eq!(
        ambiguous["last_submission_evidence"]["observed"]["echo_found"],
        false
    );
    let marker = browser_batch_marker(
        "browser-test",
        ambiguous["last_delivery_id"].as_str().expect("delivery"),
        &first.id,
    );
    assert!(
        surfaces
            .inspect_delivery_receipt(&session_key, &url, &marker)
            .await
            .expect("inspect draft")
            .is_none()
    );
    // Assistant text, another batch, and another conversation are not receipts.
    page.evaluate(format!("const a=document.createElement('article');a.dataset.messageAuthorRole='assistant';a.textContent={};document.querySelector('main').append(a)",json!(marker))).await.expect("assistant echo");
    assert!(
        surfaces
            .inspect_delivery_receipt(&session_key, &url, &marker)
            .await
            .expect("assistant ignored")
            .is_none()
    );
    let mut second = Event::new("chat.message", &group.group_id);
    second.by = "user".into();
    second.data.insert("message_mode".into(), json!("send"));
    second.data.insert("to".into(), json!(["browser-test"]));
    second
        .data
        .insert("text".into(), json!("Only later work should follow."));
    ledger::append(&ledger_path, &second).expect("second");
    page.evaluate("history.replaceState(null,'','/g/g-p-fixture/c/recovery-fixture');send();const b=document.createElement('button');b.id='generating';b.setAttribute('aria-label','Stop generating');b.textContent='Stop';document.querySelector('main').append(b)").await.expect("manual send");
    assert!(
        surfaces
            .inspect_delivery_receipt(
                &session_key,
                "https://chatgpt.com/c/other-conversation",
                &marker
            )
            .await
            .expect("wrong conversation")
            .is_none()
    );
    assert!(
        surfaces
            .inspect_delivery_receipt(
                &session_key,
                &url,
                "[cccc] Browser batch another-delivery events=other actor=other"
            )
            .await
            .expect("wrong batch")
            .is_none()
    );
    tokio::time::timeout(std::time::Duration::from_secs(10),async {
        loop {
            if load_target(&state,&group.group_id,"browser-test").expect("target")["last_delivery_status"]=="submitted" {break;}
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        }
    }).await.expect("manual send reconciled");
    let recovered = load_target(&state, &group.group_id, "browser-test").expect("recovered target");
    assert_eq!(
        recovered["bootstrap_seed_conversation_url"], url,
        "a Project-route receipt must keep the bootstrap seed on the bound URL"
    );
    let events = std::fs::read_to_string(&ledger_path)
        .expect("ledger")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("event"))
        .collect::<Vec<_>>();
    assert!(events.iter().any(|e| e["kind"] == "runtime.delivery"
        && e["data"]["source_event_id"] == first.id
        && e["data"]["state"] == "accepted"));
    assert!(
        events
            .iter()
            .any(|e| e["kind"] == "web_model.browser_delivery.submitted"
                && e["data"]["delivery_id"] == ambiguous["last_delivery_id"])
    );
    assert_eq!(
        page.evaluate("window.attempts")
            .await
            .expect("attempts")
            .into_value::<u32>()
            .expect("number"),
        1,
        "do not interrupt generation"
    );
    page.evaluate("document.querySelector('textarea').value='A new human draft';document.querySelector('#generating').remove();window.accept=true").await.expect("new draft");
    tokio::time::timeout(std::time::Duration::from_secs(10),async {
        loop {
            if load_target(&state,&group.group_id,"browser-test").expect("target")["last_delivery_status"]=="draft_blocked" {break;}
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        }
    }).await.expect("new draft protected");
    assert_eq!(
        page.evaluate("document.querySelector('textarea').value")
            .await
            .expect("draft")
            .into_value::<String>()
            .expect("text"),
        "A new human draft"
    );
    // A failed setup can leave its exact draft in the bound conversation.
    // Only the candidate for this actor has authority to identify it.
    let binding =
        super::super::web_model_connector_store::for_actor(&state, &group.group_id, "browser-test")
            .expect("binding");
    let connector = binding["connector_id"].as_str().expect("connector");
    let pairing = cccc_core::web_model_connectors::begin_pairing(
        &state.home,
        connector,
        &group.group_id,
        "browser-test",
        "fixture-generation",
        false,
    )
    .expect("start setup");
    cccc_core::web_model_connectors::fail_pairing(
        &state.home,
        connector,
        &group.group_id,
        "browser-test",
        pairing["pairing_id"].as_str().expect("candidate"),
        "pairing_interrupted",
    )
    .expect("terminal setup");
    let stale = crate::browser_surface::pairing_prompt(pairing["code"].as_str().expect("code"));
    page.evaluate(format!(
        "document.querySelector('textarea').value={}",
        json!(stale)
    ))
    .await
    .expect("put terminal setup in composer");
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if page
                .evaluate("window.sent.length")
                .await
                .expect("sent")
                .into_value::<u32>()
                .expect("number")
                == 2
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        }
    })
    .await
    .expect("later queue proceeds");
    let sent = page
        .evaluate("window.sent")
        .await
        .expect("sent")
        .into_value::<Vec<String>>()
        .expect("messages");
    assert!(sent[0].contains(&first.id));
    assert!(sent[0].contains("Session bootstrap"));
    assert!(
        !sent[1].contains("Session bootstrap"),
        "the next delivery must not seed the same conversation again"
    );
    assert!(!sent[1].contains(&first.id));
    assert!(sent[1].contains(&second.id));
    assert_eq!(
        page.evaluate("window.attempts")
            .await
            .expect("attempts")
            .into_value::<u32>()
            .expect("number"),
        2,
        "no replay or duplicate workers"
    );
    let _ = shutdown.send(());
    surfaces.close(&session_key).await.expect("close browser");
    intercept.abort();
    server.abort();
    client
        .call(&DaemonRequest {
            v: 1,
            op: "shutdown".into(),
            args: Default::default(),
        })
        .await
        .expect("stop daemon");
    daemon.await.expect("daemon task").expect("daemon stopped");
}

/// The browsers the session owner keeps, as its private IPC reports them.
async fn kept_resources(
    home: &HomeLayout,
) -> Vec<cccc_daemon::session_owner::browser::BrowserResource> {
    let response = cccc_client::DaemonClient::new(home.clone())
        .call(&DaemonRequest {
            v: 1,
            op: "browser_resources".into(),
            args: json!({"by":"user"}).as_object().expect("args").clone(),
        })
        .await
        .expect("browser resources");
    assert!(response.ok, "{:?}", response.error);
    serde_json::from_value(response.result["resources"].clone()).expect("resources")
}

/// The page records the session owner keeps for its browsers.
async fn kept_pages(home: &HomeLayout) -> Vec<String> {
    kept_resources(home)
        .await
        .into_iter()
        .flat_map(|resource| resource.sessions)
        .map(|page| page.target_id)
        .collect()
}

/// The session owner keeps the shared browser with its conversation across
/// Web and daemon restarts. Each Web holds only a CDP connection and lets go
/// when it stops; the next Web takes the same window back when it opens the
/// member's surface. A running answer holds back the next batch, and a
/// handoff interrupted after submission is recovered from the page's echo
/// without being submitted again.
#[tokio::test]
async fn preserved_browser_keeps_the_same_conversation_across_restarts() {
    use futures_util::StreamExt;
    if crate::system_browser_path().is_none()
        || !cccc_daemon::session_owner::platform_supported()
        || (cfg!(target_os = "linux") && !std::path::Path::new("/usr/bin/Xvfb").is_file())
    {
        return;
    }
    let _chrome_guard = crate::browser_surface::chrome_test_guard().await;
    let temp = tempfile::tempdir().expect("tempdir");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    home.initialize().expect("initialize");
    let store = GroupStore::new(home.clone()).expect("store");
    let mut group = store.create("preserved browser", "").expect("group");
    group.running = true;
    let mut actor = Actor::new("browser-test");
    actor.runtime = ActorRuntime::WebModel;
    actor.normalize_runtime_constraints();
    actor.generation = "fixture-generation".into();
    actor
        .env
        .insert("CCCC_WEB_MODEL_DELIVERY_MODE".into(), "browser".into());
    let identity = crate::browser_surface::actor_identity(&actor);
    let created_at = actor.created_at.clone();
    group.actors.push(actor);
    group.extra.insert(
        super::super::web_model_browser::TARGETS_KEY.into(),
        json!({}),
    );
    store.save(&group).expect("save");
    let group_id = group.group_id.clone();
    let ledger_path = store.ledger_path(&group_id).expect("ledger");
    let message = |text: &str| {
        let mut event = Event::new("chat.message", &group_id);
        event.by = "user".into();
        event.data.insert("message_mode".into(), json!("send"));
        event.data.insert("to".into(), json!(["browser-test"]));
        event.data.insert("text".into(), json!(text));
        ledger::append(&ledger_path, &event).expect("message");
        event
    };
    async fn ready(client: &cccc_client::DaemonClient) {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let ping = DaemonRequest {
                    v: 1,
                    op: "ping".into(),
                    args: Default::default(),
                };
                if client.call(&ping).await.is_ok_and(|r| r.ok) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("daemon ready");
    }
    async fn call(
        home: &HomeLayout,
        group_id: &str,
        op: &str,
        extra: Value,
    ) -> Result<Value, String> {
        let mut args = json!({"group_id":group_id,"actor_id":"browser-test","by":"browser-test"});
        args.as_object_mut()
            .expect("args")
            .extend(extra.as_object().cloned().expect("extra"));
        let response = cccc_client::DaemonClient::new(home.clone())
            .call(&DaemonRequest {
                v: 1,
                op: op.into(),
                args: args.as_object().expect("args").clone(),
            })
            .await
            .map_err(|error| error.to_string())?;
        if !response.ok {
            return Err(format!("{:?}", response.error));
        }
        Ok(Value::Object(response.result))
    }
    // The provider page: an answer runs (a visible Stop button) until the
    // test calls finish(), like a model still generating.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listen");
    let url = format!("http://{}/", listener.local_addr().expect("address"));
    let server = tokio::spawn(async move {
        axum::serve(listener, Router::new().route("/", get(|| async {
            Html(r#"<form onsubmit="event.preventDefault();window.attempts++;const t=document.querySelector('textarea');const a=document.createElement('article');a.dataset.messageAuthorRole='user';a.textContent=t.value;document.body.append(a);t.value='';document.getElementById('stop').hidden=false"><textarea id="prompt-textarea" style="width:500px;height:120px"></textarea><button type="submit">Send</button></form><button id="stop" type="button" aria-label="Stop generating" hidden>Stop</button><script>window.attempts=0;window.finish=()=>{const a=document.createElement('article');a.dataset.messageAuthorRole='assistant';a.textContent='answer '+window.attempts;document.body.append(a);document.getElementById('stop').hidden=true}</script>"#)
        }))).await.expect("serve");
    });

    // The session owner starts Chrome: it is not a child of any Web below.
    let profile = temp.path().join("profile");
    std::fs::create_dir_all(&profile).expect("profile");
    let session_key = key(&group_id, "browser-test");
    // The browser's windows, read through a connection of the test's own.
    let browser_targets = |websocket: &String| {
        let websocket = websocket.clone();
        async move {
            let (connection, mut events) = chromiumoxide::Browser::connect(websocket)
                .await
                .expect("inspect the kept browser");
            let pump = tokio::spawn(async move { while events.next().await.is_some() {} });
            let targets = connection
                .execute(chromiumoxide::cdp::browser_protocol::target::GetTargetsParams::default())
                .await
                .expect("targets")
                .result
                .target_infos
                .into_iter()
                .filter(|target| target.r#type == "page")
                .map(|target| target.target_id.inner().clone())
                .collect::<Vec<_>>();
            pump.abort();
            targets
        }
    };

    // --- First Web and daemon: deliver M1; the model is still answering.
    let daemon = tokio::spawn({
        let home = home.clone();
        async move { cccc_daemon::run(home).await }
    });
    ready(&cccc_client::DaemonClient::new(home.clone())).await;
    let shutdown = tokio::sync::broadcast::channel(1).0;
    let (_, _, surfaces, state) = crate::app_with_shutdown(
        home.clone(),
        shutdown.clone(),
        crate::WebMode::Normal,
        None,
        crate::LiveBinding {
            host: "127.0.0.1".into(),
            port: 0,
        },
        "preserved-browser-1".into(),
    );
    surfaces.hold_shared_browsers(cccc_client::DaemonClient::new(home.clone()));
    // Opening the member's surface also takes back a window kept for it.
    let open = |surfaces: Arc<crate::browser_surface::BrowserSurfaces>| {
        let (session_key, profile, url) = (session_key.clone(), profile.clone(), url.clone());
        let (identity, created_at) = (identity.clone(), created_at.clone());
        async move {
            surfaces
                .ensure_open_shared_actor(
                    &session_key,
                    &profile,
                    &url,
                    (800, 600),
                    &identity,
                    &created_at,
                )
                .await
                .expect("actor window");
        }
    };
    open(Arc::clone(&surfaces)).await;
    let websocket = {
        let mut connections = kept_resources(&home)
            .await
            .into_iter()
            .filter_map(|resource| resource.connection);
        let connection = connections.next().expect("kept browser");
        assert!(connections.next().is_none(), "one kept browser");
        connection.websocket_url
    };
    let readiness = |state: AppState| {
        let session_key = session_key.clone();
        async move {
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                loop {
                    let readiness = state
                        .browser_surfaces
                        .prompt_readiness(&session_key)
                        .await
                        .unwrap_or_default();
                    if readiness["ready"] == true {
                        break readiness;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            })
            .await
            .expect("page ready")
        }
    };
    readiness(state.clone()).await;
    let page_target = surfaces
        .sessions
        .lock()
        .await
        .get(&session_key)
        .expect("session")
        .page
        .target_id()
        .inner()
        .clone();
    let pages_before = browser_targets(&websocket).await;
    super::login_tests::pair_fixture(&state, &group_id, &url);
    update_target(
        &state,
        &group_id,
        "browser-test",
        json!({"kind":"existing_chat","url":url,"last_delivery_status":"bound"}),
    )
    .expect("target");
    let first = message("M1 before maintenance");
    assert!(matches!(
        deliver_pending(&state, &group_id, "browser-test")
            .await
            .expect("deliver M1"),
        DeliveryOutcome::Submitted
    ));
    let page = surfaces
        .sessions
        .lock()
        .await
        .get(&session_key)
        .expect("session")
        .page
        .clone();
    let attempts = |page: chromiumoxide::Page| async move {
        page.evaluate("window.attempts")
            .await
            .expect("attempts")
            .into_value::<u32>()
            .expect("number")
    };
    assert_eq!(attempts(page.clone()).await, 1);
    assert_eq!(
        state
            .browser_surfaces
            .prompt_readiness(&session_key)
            .await
            .expect("readiness")["running"],
        true,
        "the model is still answering M1"
    );

    // --- Maintenance: the Web stops and lets go, and the daemon stops
    // keeping its members; the session owner keeps the window recorded.
    crate::shutdown::browser_surfaces(&surfaces).await;
    let _ = shutdown.send(());
    drop(state);
    drop(surfaces);
    assert_eq!(kept_pages(&home).await, [page_target.clone()]);
    call(&home, &group_id, "shutdown_preserving_agents", json!({}))
        .await
        .expect("stop keeping members");
    daemon.await.expect("daemon task").expect("daemon stopped");
    assert_eq!(
        browser_targets(&websocket).await,
        pages_before,
        "no window closed or added"
    );

    // --- Second Web and daemon: the same window comes back; M2 waits for
    // the running answer and is then delivered once.
    let daemon = tokio::spawn({
        let home = home.clone();
        async move { cccc_daemon::run(home).await }
    });
    ready(&cccc_client::DaemonClient::new(home.clone())).await;
    let shutdown = tokio::sync::broadcast::channel(1).0;
    let (_, _, surfaces, state) = crate::app_with_shutdown(
        home.clone(),
        shutdown.clone(),
        crate::WebMode::Normal,
        None,
        crate::LiveBinding {
            host: "127.0.0.1".into(),
            port: 0,
        },
        "preserved-browser-2".into(),
    );
    surfaces.hold_shared_browsers(cccc_client::DaemonClient::new(home.clone()));
    open(Arc::clone(&surfaces)).await;
    assert!(state.browser_surfaces.info(&session_key).await["active"] == true);
    let page = surfaces
        .sessions
        .lock()
        .await
        .get(&session_key)
        .expect("restored session")
        .page
        .clone();
    assert_eq!(page.target_id().inner(), &page_target);
    assert_eq!(attempts(page.clone()).await, 1, "the page kept its state");
    let second = message("M2 during the running answer");
    assert!(matches!(
        deliver_pending(&state, &group_id, "browser-test")
            .await
            .expect("wait for the answer"),
        DeliveryOutcome::Idle
    ));
    assert_eq!(attempts(page.clone()).await, 1, "M2 waits for the answer");
    page.evaluate("window.finish()").await.expect("answer done");
    readiness(state.clone()).await;
    assert!(matches!(
        deliver_pending(&state, &group_id, "browser-test")
            .await
            .expect("deliver M2"),
        DeliveryOutcome::Submitted
    ));
    assert_eq!(attempts(page.clone()).await, 2);
    page.evaluate("window.finish()").await.expect("answer done");
    readiness(state.clone()).await;

    // --- An interrupted handoff: the Web claimed M3, wrote its fence and
    // submitted it, then stopped before recording the outcome.
    let third = message("M3 interrupted after submission");
    let wait = call(
        &home,
        &group_id,
        "runtime_wait_next_turn",
        json!({"transport":"web_model_browser"}),
    )
    .await
    .expect("claim M3");
    let turn = wait["turn"].clone();
    assert_eq!(turn["event_ids"], json!([third.id]));
    let turn_id = turn["turn_id"].as_str().expect("turn").to_owned();
    let delivery_id = browser_delivery_id("browser-test", &turn_id);
    let target = load_target(&state, &group_id, "browser-test").expect("target");
    let (prompt, _) = build_browser_prompt(
        &turn,
        &target,
        &url,
        "browser-test",
        &delivery_id,
        &third.id,
    )
    .expect("prompt");
    update_target(
        &state,
        &group_id,
        "browser-test",
        json!({"last_delivery_id":delivery_id,"last_delivery_turn_id":turn_id,
               "last_delivery_event_ids":turn["event_ids"],"last_delivery_status":"submitting"}),
    )
    .expect("fence");
    record_delivery(
        &state,
        &group_id,
        "browser-test",
        &turn_id,
        turn["event_ids"].clone(),
        &delivery_id,
        "submitting",
        "",
        json!({"target_url":url}),
    )
    .await;
    page.evaluate(format!(
        "document.querySelector('textarea').value={};document.querySelector('form').requestSubmit()",
        json!(prompt)
    ))
    .await
    .expect("submitted before the Web stopped");
    assert_eq!(attempts(page.clone()).await, 3);
    crate::shutdown::browser_surfaces(&surfaces).await;
    let _ = shutdown.send(());
    drop(state);
    drop(surfaces);
    call(&home, &group_id, "shutdown_preserving_agents", json!({}))
        .await
        .expect("stop keeping members again");
    daemon.await.expect("daemon task").expect("daemon stopped");

    // --- Third Web and daemon: the same window proves M3 arrived.
    let daemon = tokio::spawn({
        let home = home.clone();
        async move { cccc_daemon::run(home).await }
    });
    ready(&cccc_client::DaemonClient::new(home.clone())).await;
    let shutdown = tokio::sync::broadcast::channel(1).0;
    let (_, _, surfaces, state) = crate::app_with_shutdown(
        home.clone(),
        shutdown.clone(),
        crate::WebMode::Normal,
        None,
        crate::LiveBinding {
            host: "127.0.0.1".into(),
            port: 0,
        },
        "preserved-browser-3".into(),
    );
    surfaces.hold_shared_browsers(cccc_client::DaemonClient::new(home.clone()));
    open(Arc::clone(&surfaces)).await;
    // The model is still answering M3 while this Web comes back.
    let page = surfaces
        .sessions
        .lock()
        .await
        .get(&session_key)
        .expect("restored session")
        .page
        .clone();
    assert_eq!(page.target_id().inner(), &page_target);
    let mut outcomes = Vec::new();
    for _ in 0..3 {
        let outcome = deliver_pending(&state, &group_id, "browser-test")
            .await
            .expect("recover M3");
        let done = matches!(outcome, DeliveryOutcome::Submitted);
        outcomes.push(match outcome {
            DeliveryOutcome::Submitted => "submitted".to_owned(),
            DeliveryOutcome::Idle => "idle".to_owned(),
            DeliveryOutcome::Deferred(turn) => format!("deferred {turn}"),
            DeliveryOutcome::Ambiguous => "ambiguous".to_owned(),
            DeliveryOutcome::Stopped => "stopped".to_owned(),
        });
        if done {
            break;
        }
    }
    let target = load_target(&state, &group_id, "browser-test").expect("target");
    assert_eq!(
        attempts(page.clone()).await,
        3,
        "M3 was never submitted again"
    );
    for event in [&first, &second, &third] {
        let states = cccc_core::inbox::delivery_states(
            &home,
            &store.load(&group_id).expect("group"),
            &["browser-test".into()],
            &event.id,
        )
        .expect("states");
        assert_eq!(
            states.get("browser-test"),
            Some(&("accepted".to_owned(), "web_model_browser".to_owned())),
            "{}",
            event.id
        );
    }
    assert_eq!(outcomes, ["ambiguous", "submitted"]);
    assert_eq!(target["last_delivery_status"], "submitted", "{outcomes:?}");
    assert_eq!(
        browser_targets(&websocket).await,
        pages_before,
        "still the same windows"
    );

    // Stopping CCCC stops the browser the session owner kept.
    crate::shutdown::browser_surfaces(&surfaces).await;
    let _ = shutdown.send(());
    call(&home, &group_id, "shutdown", json!({}))
        .await
        .expect("final stop");
    daemon.await.expect("daemon task").expect("daemon stopped");
    server.abort();
}

/// A Web that is stopping claims no more browser work, and lets go of its
/// browser only after the work it already claimed has reached its record.
/// The session owner keeps Chrome, so the first Web can hand the window to
/// the second.
#[tokio::test]
async fn a_stopping_web_records_claimed_work_and_claims_no_more() {
    if crate::system_browser_path().is_none()
        || !cccc_daemon::session_owner::platform_supported()
        || (cfg!(target_os = "linux") && !std::path::Path::new("/usr/bin/Xvfb").is_file())
    {
        return;
    }
    let _chrome_guard = crate::browser_surface::chrome_test_guard().await;
    let temp = tempfile::tempdir().expect("tempdir");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    home.initialize().expect("initialize");
    let store = GroupStore::new(home.clone()).expect("store");
    let mut group = store.create("stopping web", "").expect("group");
    group.running = true;
    let mut actor = Actor::new("browser-test");
    actor.runtime = ActorRuntime::WebModel;
    actor.normalize_runtime_constraints();
    actor.generation = "fixture-generation".into();
    actor
        .env
        .insert("CCCC_WEB_MODEL_DELIVERY_MODE".into(), "browser".into());
    let identity = crate::browser_surface::actor_identity(&actor);
    let created_at = actor.created_at.clone();
    group.actors.push(actor);
    group.extra.insert(
        super::super::web_model_browser::TARGETS_KEY.into(),
        json!({}),
    );
    store.save(&group).expect("save");
    let group_id = group.group_id.clone();
    let mut message = Event::new("chat.message", &group_id);
    message.by = "user".into();
    message.data.insert("message_mode".into(), json!("send"));
    message.data.insert("to".into(), json!(["browser-test"]));
    message
        .data
        .insert("text".into(), json!("M1 while stopping"));

    // The provider page shows a sent message a second after Send, so the
    // delivery is still waiting for that evidence when the Web is asked to stop.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listen");
    let url = format!("http://{}/", listener.local_addr().expect("address"));
    let server = tokio::spawn(async move {
        axum::serve(listener, Router::new().route("/", get(|| async {
            Html(r#"<form onsubmit="event.preventDefault();window.attempts++;const t=document.querySelector('textarea');const text=t.value;t.value='';setTimeout(()=>{const a=document.createElement('article');a.dataset.messageAuthorRole='user';a.textContent=text;document.body.append(a)},1000)"><textarea id="prompt-textarea" style="width:500px;height:120px"></textarea><button type="submit">Send</button></form><script>window.attempts=0</script>"#)
        }))).await.expect("serve");
    });
    let profile = temp.path().join("profile");
    std::fs::create_dir_all(&profile).expect("profile");
    let daemon = tokio::spawn({
        let home = home.clone();
        async move { cccc_daemon::run(home).await }
    });
    let client = cccc_client::DaemonClient::new(home.clone());
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let ping = DaemonRequest {
            v: 1,
            op: "ping".into(),
            args: Default::default(),
        };
        while !client.call(&ping).await.is_ok_and(|r| r.ok) {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("daemon ready");
    // Each Web leaves the shared browser to the session owner, as `serve` does.
    let web = |name: &str| {
        let web = crate::app_with_shutdown(
            home.clone(),
            tokio::sync::broadcast::channel(1).0,
            crate::WebMode::Normal,
            None,
            crate::LiveBinding {
                host: "127.0.0.1".into(),
                port: 0,
            },
            name.into(),
        );
        web.2.hold_shared_browsers(client.clone());
        web
    };
    let session_key = key(&group_id, "browser-test");
    // Opening the member's surface also takes back a window kept for it.
    let open = |surfaces: Arc<crate::browser_surface::BrowserSurfaces>| {
        let (session_key, profile, url) = (session_key.clone(), profile.clone(), url.clone());
        let (identity, created_at) = (identity.clone(), created_at.clone());
        async move {
            surfaces
                .ensure_open_shared_actor(
                    &session_key,
                    &profile,
                    &url,
                    (800, 600),
                    &identity,
                    &created_at,
                )
                .await
                .expect("actor window");
        }
    };
    let ready = |state: AppState| {
        let session_key = session_key.clone();
        async move {
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while state
                    .browser_surfaces
                    .prompt_readiness(&session_key)
                    .await
                    .unwrap_or_default()["ready"]
                    != true
                {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            })
            .await
            .expect("page ready");
        }
    };

    // --- The first Web opens the window, then stops: M1 stays queued.
    let (_, _, surfaces, state) = web("stopping-web-1");
    open(Arc::clone(&surfaces)).await;
    ready(state.clone()).await;
    super::login_tests::pair_fixture(&state, &group_id, &url);
    update_target(
        &state,
        &group_id,
        "browser-test",
        json!({"kind":"existing_chat","url":url,"last_delivery_status":"bound"}),
    )
    .expect("target");
    ledger::append(&store.ledger_path(&group_id).expect("ledger"), &message).expect("M1");
    surfaces.stop_opening();
    assert!(matches!(
        deliver_pending(&state, &group_id, "browser-test")
            .await
            .expect("stopping Web"),
        DeliveryOutcome::Idle
    ));
    let target = load_target(&state, &group_id, "browser-test").expect("target");
    assert_eq!(
        target["last_delivery_status"], "bound",
        "M1 was not claimed"
    );
    crate::shutdown::browser_surfaces(&surfaces).await;
    drop(state);
    drop(surfaces);

    // --- The second Web takes the window back, claims M1 and is asked to
    // stop while submitting it.
    let (_, _, surfaces, state) = web("stopping-web-2");
    open(Arc::clone(&surfaces)).await;
    ready(state.clone()).await;
    let delivery = tokio::spawn({
        let state = state.clone();
        let group_id = group_id.clone();
        async move { deliver_pending(&state, &group_id, "browser-test").await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while load_target(&state, &group_id, "browser-test").expect("target")
            ["last_delivery_status"]
            != "submitting"
        {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("M1 claimed");
    let stopping = std::time::Instant::now();
    crate::shutdown::browser_surfaces(&surfaces).await;
    assert!(
        delivery.is_finished(),
        "the browser was let go only after the claimed work ended"
    );
    assert!(stopping.elapsed() < std::time::Duration::from_secs(10));
    assert!(matches!(
        delivery.await.expect("delivery task").expect("delivery"),
        DeliveryOutcome::Submitted
    ));
    let target = load_target(&state, &group_id, "browser-test").expect("target");
    assert_eq!(target["last_delivery_status"], "submitted");
    let states = cccc_core::inbox::delivery_states(
        &home,
        &store.load(&group_id).expect("group"),
        &["browser-test".into()],
        &message.id,
    )
    .expect("states");
    assert_eq!(
        states.get("browser-test"),
        Some(&("accepted".to_owned(), "web_model_browser".to_owned()))
    );

    drop(state);
    drop(surfaces);
    client
        .call(&DaemonRequest {
            v: 1,
            op: "shutdown".into(),
            args: Default::default(),
        })
        .await
        .expect("stop daemon");
    daemon.await.expect("daemon task").expect("daemon stopped");
    server.abort();
}

const RECOVERY_ACTOR: &str = "browser-test";

struct RecoveryFixture {
    _home: tempfile::TempDir,
    state: AppState,
    group: cccc_core::GroupDoc,
}

impl RecoveryFixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(directory.path().join("home")).expect("home");
        home.initialize().expect("initialize");
        let store = GroupStore::new(home.clone()).expect("store");
        let mut group = store.create("page recovery", "").expect("group");
        group.running = true;
        let mut actor = Actor::new(RECOVERY_ACTOR);
        actor.runtime = ActorRuntime::WebModel;
        actor.normalize_runtime_constraints();
        actor.generation = "fixture-generation".into();
        actor
            .env
            .insert("CCCC_WEB_MODEL_DELIVERY_MODE".into(), "browser".into());
        group.actors.push(actor);
        group.extra.insert(
            super::super::web_model_browser::TARGETS_KEY.into(),
            json!({}),
        );
        store.save(&group).expect("save");
        let (_, _, _, state) = crate::app_with_shutdown(
            home,
            tokio::sync::broadcast::channel(1).0,
            crate::WebMode::Normal,
            None,
            crate::LiveBinding {
                host: "127.0.0.1".into(),
                port: 0,
            },
            "managed-page-recovery".into(),
        );
        Self {
            _home: directory,
            state,
            group,
        }
    }

    fn queue(&self) -> Event {
        let store = GroupStore::new(self.state.home.clone()).expect("store");
        let mut event = Event::new("chat.message", &self.group.group_id);
        event.by = "user".into();
        event.data.insert("message_mode".into(), json!("send"));
        event.data.insert("to".into(), json!([RECOVERY_ACTOR]));
        event
            .data
            .insert("text".into(), json!("Queued while the browser was closed"));
        ledger::append(
            &store.ledger_path(&self.group.group_id).expect("ledger"),
            &event,
        )
        .expect("message");
        event
    }

    async fn open(&self, url: &str) -> String {
        let group = &self.group.group_id;
        super::login_tests::pair_fixture(&self.state, group, url);
        let actor = &self.group.actors[0];
        let profile = super::super::web_model_shared_browser::profile(&self.state, "chatgpt_web");
        let session_key = key(group, RECOVERY_ACTOR);
        self.state
            .browser_surfaces
            .open_shared_actor_fixture(
                &session_key,
                &profile,
                url,
                &crate::browser_surface::actor_identity(actor),
            )
            .await
            .expect("managed browser page");
        self.wait_for_page(&session_key, url).await;
        session_key
    }

    async fn wait_for_page(&self, key: &str, url: &str) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if self
                    .state
                    .browser_surfaces
                    .prompt_readiness(key)
                    .await
                    .is_ok_and(|state| state["tab_url"] == url && state["state"] != "loading")
                {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("fixture page loads");
    }
}

async fn local_recovery_server(router: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listen");
    let url = format!("http://{}/", listener.local_addr().expect("address"));
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve");
    });
    (url, server)
}

#[tokio::test]
async fn closed_idle_and_unresponsive_pages_reopen_only_on_their_own_managed_target() {
    use super::super::web_model_supervisor::reopen_on_demand;
    use std::time::Duration;

    if crate::system_browser_path().is_none() {
        return;
    }
    let _chrome_guard = crate::browser_surface::chrome_test_guard().await;
    let (url, server) = local_recovery_server(Router::new().route("/", get(|| async {
        Html(r#"<textarea id="prompt-textarea" style="width:500px;height:120px"></textarea>
        <button aria-label="Stop generating">Stop</button><script>document.hasFocus=()=>false;</script>"#)
    }))).await;
    let fixture = RecoveryFixture::new();
    let state = &fixture.state;
    let group = &fixture.group.group_id;
    let session_key = fixture.open(&url).await;
    let surfaces = &state.browser_surfaces;
    let profile = super::super::web_model_shared_browser::profile(state, "chatgpt_web");
    let sibling = surfaces
        .open_shared_actor_fixture("test-sibling", &profile, &url, "sibling")
        .await
        .expect("sibling");
    let initial = surfaces.info(&session_key).await;
    let initial_target = initial["target_id"].as_str().expect("target");
    assert!(
        !surfaces
            .consider_actor_idle_close(&session_key, initial_target, true, Duration::ZERO)
            .await
            .expect("busy page protected")
    );
    let page = surfaces
        .sessions
        .lock()
        .await
        .get(&session_key)
        .expect("session")
        .page
        .clone();
    page.evaluate("document.querySelector('button').remove()")
        .await
        .expect("finish response");
    assert!(
        surfaces
            .consider_actor_idle_close(&session_key, initial_target, true, Duration::ZERO)
            .await
            .expect("idle page closes")
    );
    let retired = surfaces.info(&session_key).await;
    assert_eq!(retired["closed_reason"], "idle");
    assert!(!reopen_on_demand(state, group, RECOVERY_ACTOR, &retired).await);
    assert_eq!(surfaces.info(&session_key).await["active"], false);

    fixture.queue();
    assert!(reopen_on_demand(state, group, RECOVERY_ACTOR, &retired).await);
    fixture.wait_for_page(&session_key, &url).await;
    let first_reopened = surfaces.info(&session_key).await;
    let first_reopened_id = first_reopened["target_id"].as_str().expect("new target");
    assert_ne!(first_reopened_id, initial_target);
    assert_ne!(first_reopened["target_id"], sibling["target_id"]);
    let page = surfaces
        .sessions
        .lock()
        .await
        .get(&session_key)
        .expect("session")
        .page
        .clone();
    page.evaluate("document.querySelector('button').remove()")
        .await
        .expect("finish response");
    surfaces
        .prompt_readiness(&session_key)
        .await
        .expect("last healthy observation");
    for expected in [false, false, true] {
        assert_eq!(
            surfaces
                .actor_probe_timed_out(&session_key, first_reopened_id, true)
                .await
                .expect("renderer timeout"),
            expected
        );
    }
    let retired = surfaces.info(&session_key).await;
    assert_eq!(retired["closed_reason"], "renderer_unresponsive");
    assert!(reopen_on_demand(state, group, RECOVERY_ACTOR, &retired).await);
    assert_ne!(
        surfaces.info(&session_key).await["target_id"],
        first_reopened["target_id"]
    );
    assert_eq!(
        surfaces.info("test-sibling").await["target_id"],
        sibling["target_id"]
    );
    surfaces.shutdown_all().await.expect("close shared browser");
    server.abort();
}

#[tokio::test]
async fn renderer_loss_reopens_the_bound_chat_and_delivers_once() {
    use chromiumoxide::cdp::browser_protocol::{
        inspector::{EnableParams, EventTargetCrashed},
        page::CrashParams,
    };
    use futures_util::StreamExt;
    use std::time::Duration;

    if crate::system_browser_path().is_none() {
        return;
    }
    let _chrome_guard = crate::browser_surface::chrome_test_guard().await;
    let (base, server) = local_recovery_server(
        Router::new()
            .route("/", get(|| async { Html("<p>Neighboring tab</p>") }))
            .route("/c/renderer-recovery", get(|| async {
                Html(r#"<main><form onsubmit="event.preventDefault();window.attempts++;const t=document.querySelector('textarea');const a=document.createElement('article');a.dataset.messageAuthorRole='user';a.textContent=t.value;document.querySelector('main').append(a);t.value=''"><textarea id="prompt-textarea" style="width:500px;height:120px"></textarea><button type="submit">Send</button></form></main><script>window.attempts=0</script>"#)
            })),
    )
    .await;
    let url = format!("{base}c/renderer-recovery");
    let fixture = RecoveryFixture::new();
    let state = &fixture.state;
    let group = &fixture.group.group_id;
    let daemon = crate::test_daemon::TestDaemon::start(&state.home).await;
    let session_key = fixture.open(&url).await;
    let surfaces = &state.browser_surfaces;
    assert_eq!(
        surfaces
            .prompt_readiness(&session_key)
            .await
            .expect("healthy composer")["ready"],
        true
    );
    let binding = || {
        super::super::web_model_connector_store::for_actor(state, group, RECOVERY_ACTOR)
            .expect("saved binding")
    };
    let revision = binding()["revision"].clone();
    let profile = super::super::web_model_shared_browser::profile(state, "chatgpt_web");
    let sibling_url = base.replacen("127.0.0.1", "localhost", 1);
    let sibling = surfaces
        .open_shared_actor_fixture("renderer-loss-sibling", &profile, &sibling_url, "sibling")
        .await
        .expect("neighboring page");
    fixture
        .wait_for_page("renderer-loss-sibling", &sibling_url)
        .await;
    let initial = surfaces.info(&session_key).await;
    let page = surfaces.sessions.lock().await[&session_key].page.clone();
    let mut crashes = page
        .event_listener::<EventTargetCrashed>()
        .await
        .expect("crash events");
    page.execute(EnableParams::default())
        .await
        .expect("enable crash events");
    // Crash the renderer for this tab in the fixture browser. Chrome leaves the
    // target registered, so this exercises readiness instead of target removal.
    let crash = tokio::spawn(async move { page.execute(CrashParams::default()).await });
    tokio::time::timeout(Duration::from_secs(10), crashes.next())
        .await
        .expect("renderer crash notification timed out")
        .expect("renderer crashed");
    crash.abort();
    let listed = surfaces.info(&session_key).await;
    assert_eq!(
        listed["active"], true,
        "crashed target must still be listed"
    );
    assert_eq!(listed["target_id"], initial["target_id"]);
    let queued = fixture.queue();

    for _ in 0..2 {
        assert!(matches!(
            deliver_pending(state, group, RECOVERY_ACTOR)
                .await
                .expect("inspect crashed page"),
            DeliveryOutcome::Idle
        ));
        let waiting = surfaces.info(&session_key).await;
        assert_eq!(waiting["target_id"], initial["target_id"]);
        assert_eq!(
            waiting["metadata"]["prompt_readiness"]["state"],
            "inspection_failed"
        );
    }
    assert!(matches!(
        deliver_pending(state, group, RECOVERY_ACTOR)
            .await
            .expect("retire crashed page"),
        DeliveryOutcome::Idle
    ));
    let retired = surfaces.info(&session_key).await;
    assert_eq!(retired["closed_reason"], "renderer_unresponsive");
    assert!(
        super::super::web_model_supervisor::reopen_on_demand(
            state,
            group,
            RECOVERY_ACTOR,
            &retired
        )
        .await
    );
    fixture.wait_for_page(&session_key, &url).await;
    assert_ne!(
        surfaces.info(&session_key).await["target_id"],
        initial["target_id"]
    );
    assert_eq!(binding()["url"], url);
    assert_eq!(binding()["revision"], revision);
    assert_eq!(
        surfaces.info("renderer-loss-sibling").await["target_id"],
        sibling["target_id"]
    );
    fixture
        .wait_for_page("renderer-loss-sibling", &sibling_url)
        .await;

    assert!(matches!(
        deliver_pending(state, group, RECOVERY_ACTOR)
            .await
            .expect("deliver queued event"),
        DeliveryOutcome::Submitted
    ));
    assert!(matches!(
        deliver_pending(state, group, RECOVERY_ACTOR)
            .await
            .expect("check for replay"),
        DeliveryOutcome::Idle
    ));
    let page = surfaces.sessions.lock().await[&session_key].page.clone();
    assert_eq!(
        page.evaluate("window.attempts")
            .await
            .expect("attempts")
            .into_value::<u32>()
            .expect("count"),
        1
    );
    let target = load_target(state, group, RECOVERY_ACTOR).expect("settled target");
    assert_eq!(target["last_delivery_status"], "submitted");
    assert_eq!(target["last_delivery_event_ids"], json!([queued.id]));
    let store = GroupStore::new(state.home.clone()).expect("store");
    let events =
        std::fs::read_to_string(store.ledger_path(group).expect("ledger path")).expect("ledger");
    let settlements = events
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("ledger event"))
        .filter(|event| {
            event["kind"] == "runtime.delivery"
                && event["data"]["source_event_id"] == queued.id
                && event["data"]["state"] == "accepted"
        })
        .count();
    assert_eq!(settlements, 1);
    surfaces
        .shutdown_all()
        .await
        .expect("close fixture browser");
    server.abort();
    daemon.stop().await;
}

#[tokio::test]
async fn load_error_is_retried_before_ambiguous_review_and_exhausts_visibly() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    if crate::system_browser_path().is_none() {
        return;
    }
    let _chrome_guard = crate::browser_surface::chrome_test_guard().await;
    let visits = Arc::new(AtomicUsize::new(0));
    let visits_on_page = Arc::clone(&visits);
    let healthy = Arc::new(AtomicBool::new(false));
    let healthy_on_page = Arc::clone(&healthy);
    let (url, server) = local_recovery_server(Router::new().route("/", get(move || {
        let visits = Arc::clone(&visits_on_page);
        let healthy = Arc::clone(&healthy_on_page);
        async move {
            use axum::response::IntoResponse;
            visits.fetch_add(1, Ordering::Relaxed);
            if healthy.load(Ordering::Relaxed) {
                return Html(r#"<article data-message-author-role="user">Earlier message</article>
                    <textarea id="prompt-textarea" style="width:500px;height:120px"></textarea>"#)
                    .into_response();
            }
            (axum::http::StatusCode::TOO_MANY_REQUESTS, [("Retry-After", "85")],
                Html("<main>Could not load this ChatGPT conversation<button>Try again</button></main>"))
                .into_response()
        }
    }))).await;
    let fixture = RecoveryFixture::new();
    let state = &fixture.state;
    let group = &fixture.group.group_id;
    let session_key = fixture.open(&url).await;
    fixture.queue();
    update_target(
        state,
        group,
        RECOVERY_ACTOR,
        json!({
            "last_delivery_status":"submission_ambiguous","last_delivery_id":"prior"
        }),
    )
    .expect("existing fence");
    let binding =
        load_target(state, group, RECOVERY_ACTOR).expect("bound")["binding_revision"].clone();

    let page = state
        .browser_surfaces
        .sessions
        .lock()
        .await
        .get(&session_key)
        .expect("page")
        .page
        .clone();
    page.evaluate(
        r#"() => {
        const input = document.createElement('input');
        input.type = 'file';
        input.id = 'held-file';
        const upload = new DataTransfer();
        upload.items.add(new File(['draft'], 'draft.txt'));
        input.files = upload.files;
        document.body.append(input);
        const stop = document.createElement('button');
        stop.id = 'running-reply';
        stop.setAttribute('aria-label', 'Stop generating');
        stop.textContent = 'Stop';
        document.body.append(stop);
    }"#,
    )
    .await
    .expect("staged file while provider stops");
    let readiness = state
        .browser_surfaces
        .prompt_readiness(&session_key)
        .await
        .expect("load error");
    assert_eq!(readiness["ready"], false);
    assert_eq!(readiness["composer_has_attachments"], true);
    assert_eq!(readiness["running"], true);
    assert!(matches!(
        deliver_pending(state, group, RECOVERY_ACTOR)
            .await
            .expect("record episode"),
        DeliveryOutcome::Idle
    ));
    let mut due =
        load_target(state, group, RECOVERY_ACTOR).expect("target")["load_recovery"].clone();
    due["next_reload_at_ms"] = json!(0);
    update_target(state, group, RECOVERY_ACTOR, json!({"load_recovery":due})).expect("due retry");
    for blocker in ["#running-reply", "#held-file"] {
        assert!(matches!(
            deliver_pending(state, group, RECOVERY_ACTOR)
                .await
                .expect("busy error page"),
            DeliveryOutcome::Idle
        ));
        assert_eq!(visits.load(Ordering::Relaxed), 1);
        assert_eq!(
            load_target(state, group, RECOVERY_ACTOR).expect("target")["load_recovery"]["reload_attempts"],
            0,
            "a blocked retry must not spend its budget"
        );
        page.evaluate(format!(
            "document.querySelector({}).remove()",
            json!(blocker)
        ))
        .await
        .expect("operator clears obstruction");
    }

    for attempts in 1..=3 {
        assert!(matches!(
            deliver_pending(state, group, RECOVERY_ACTOR)
                .await
                .expect("load error"),
            DeliveryOutcome::Idle
        ));
        let target = load_target(state, group, RECOVERY_ACTOR).expect("target");
        assert_eq!(target["binding_revision"], binding);
        assert_eq!(target["last_delivery_status"], "submission_ambiguous");
        assert_eq!(target["load_recovery"]["reload_attempts"], attempts);
        if attempts < 3 {
            let mut due = target["load_recovery"].clone();
            due["next_reload_at_ms"] = json!(0);
            update_target(state, group, RECOVERY_ACTOR, json!({"load_recovery":due}))
                .expect("advance retry clock without sleeping");
        } else {
            assert!(target["load_recovery"]["next_reload_at_ms"].is_null());
        }
    }
    let attempted = visits.load(Ordering::Relaxed);
    assert_eq!(attempted, 4);
    assert!(matches!(
        deliver_pending(state, group, RECOVERY_ACTOR)
            .await
            .expect("exhausted"),
        DeliveryOutcome::Idle
    ));
    assert_eq!(visits.load(Ordering::Relaxed), attempted);
    let health = super::super::web_model_browser::payload(state, group, RECOVERY_ACTOR, false)
        .await
        .expect("health")
        .0["result"]["health_snapshot"]
        .clone();
    assert_eq!(health["browser"]["state"], "load_recovery_failed");
    assert!(health["delivery"]["oldest_undelivered_at"].is_string());

    // An operator reopens the conversation after the provider is healthy.
    healthy.store(true, Ordering::Relaxed);
    page.reload().await.expect("operator reload");
    assert!(matches!(
        deliver_pending(state, group, RECOVERY_ACTOR)
            .await
            .expect("healthy page"),
        DeliveryOutcome::Ambiguous
    ));
    assert!(
        load_target(state, group, RECOVERY_ACTOR).expect("new target")["load_recovery"].is_null()
    );
    state
        .browser_surfaces
        .close(&session_key)
        .await
        .expect("close");
    state
        .browser_surfaces
        .shutdown_all()
        .await
        .expect("close shared browser");
    server.abort();
}

#[test]
fn a_reopened_chat_with_a_composer_still_requires_bound_thread_evidence() {
    let target = json!({"kind":"existing_chat","url":"https://chatgpt.com/c/thread"});
    let mut readiness =
        json!({"ready":true,"running":false,"tab_url":"https://chatgpt.com/c/thread"});
    assert!(!ready_to_claim(&target, &readiness));
    readiness["thread_loaded"] = json!(true);
    readiness["tab_url"] = json!("https://chatgpt.com/g/g-p-project/c/thread");
    assert!(ready_to_claim(&target, &readiness));
    readiness["tab_url"] = json!("https://chatgpt.com/c/other");
    assert!(!ready_to_claim(&target, &readiness));
}
