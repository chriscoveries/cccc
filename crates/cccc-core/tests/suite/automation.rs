// Included by the crate-level integration test harness.
use cccc_contracts::{Actor, Event, GroupState};
use cccc_core::{GroupStore, HomeLayout, actors, automation, ledger};
use serde_json::json;
use std::collections::HashSet;

#[test]
fn canonical_interval_rule_starts_its_clock_and_emits_once_when_due() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    let store = GroupStore::new(home.clone()).expect("store");
    let group = store.create("automation", "").expect("group");
    store
        .mutate(&group.group_id, |group| {
            actors::add(group, Actor::new("peer"))?;
            group.automation = json!({
                "version":1,
                "rules":[{
                    "id":"reminder","enabled":true,"to":["@all"],
                    "trigger":{"kind":"interval","every_seconds":3600},
                    "action":{"kind":"notify","message":"check in"}
                }]
            })
            .as_object()
            .cloned()
            .expect("object");
            Ok(())
        })
        .expect("automation config");

    let first = automation::tick(&home).expect("first tick");
    assert!(first.notifications.is_empty());
    let state_path = store
        .state_dir(&group.group_id)
        .expect("state dir")
        .join("automation.json");
    let mut state: serde_json::Value =
        cccc_core::fs::read_json(&state_path).expect("automation state");
    state["rules"]["reminder"]["last_fired_at"] = json!("2020-01-01T00:00:00Z");
    cccc_core::fs::write_json(&state_path, &state).expect("due state");

    let due = automation::tick(&home).expect("due tick");
    assert_eq!(due.notifications.len(), 1);
    assert_eq!(due.notifications[0].data["message"], "check in");
    let repeated = automation::tick(&home).expect("repeated tick");
    assert!(repeated.notifications.is_empty());
}

#[test]
fn idle_group_suppresses_builtin_standup_but_runs_custom_rules() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    let store = GroupStore::new(home.clone()).expect("store");
    let group = store.create("idle automation", "").expect("group");
    store
        .mutate(&group.group_id, |group| {
            group.state = GroupState::Idle;
            actors::add(group, Actor::new("peer"))?;
            group.automation = json!({
                "version":1,
                "rules":[
                    {
                        "id":"standup","enabled":true,"to":["@all"],
                        "trigger":{"kind":"interval","every_seconds":60},
                        "action":{"kind":"notify","message":"built in"}
                    },
                    {
                        "id":"custom","enabled":true,"to":["@all"],
                        "trigger":{"kind":"interval","every_seconds":60},
                        "action":{"kind":"notify","message":"custom"}
                    }
                ]
            })
            .as_object()
            .cloned()
            .expect("object");
            Ok(())
        })
        .expect("automation config");

    let baseline = automation::tick_group(&home, &group.group_id, false).expect("baseline tick");
    assert!(baseline.notifications.is_empty());
    let state_path = store
        .state_dir(&group.group_id)
        .expect("state dir")
        .join("automation.json");
    let mut state: serde_json::Value =
        cccc_core::fs::read_json(&state_path).expect("automation state");
    state["rules"]["standup"]["last_fired_at"] = json!("2020-01-01T00:00:00Z");
    state["rules"]["custom"]["last_fired_at"] = json!("2020-01-01T00:00:00Z");
    cccc_core::fs::write_json(&state_path, &state).expect("due state");

    let due = automation::tick_group(&home, &group.group_id, false).expect("idle tick");
    assert_eq!(due.notifications.len(), 1);
    assert_eq!(due.notifications[0].data["context"]["rule_id"], "custom");
}

#[test]
fn mail_notice_waits_for_a_delivery_eligible_actor_and_is_one_shot() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    let store = GroupStore::new(home.clone()).expect("store");
    let group = store.create("automation", "").expect("group");
    store
        .mutate(&group.group_id, |group| {
            group.state = GroupState::Active;
            actors::add(group, Actor::new("peer"))?;
            group
                .extra
                .insert("delivery".into(), json!({"mail_notice_after_seconds":1}));
            Ok(())
        })
        .expect("delivery settings");
    let mut message = Event::new("chat.message", &group.group_id);
    message.by = "user".into();
    message.ts = "2020-01-01T00:00:00Z".into();
    message.data = json!({
        "text":"private work detail that must not be copied into a notice",
        "to":["peer"],
        "message_mode":"mail"
    })
    .as_object()
    .cloned()
    .expect("message");
    ledger::append(
        &store.ledger_path(&group.group_id).expect("ledger"),
        &message,
    )
    .expect("append unread message");

    let none =
        automation::tick_group_for_delivery_actors(&home, &group.group_id, true, &HashSet::new())
            .expect("stopped actor tick");
    assert!(none.notifications.is_empty());

    let eligible = HashSet::from(["peer".to_owned()]);
    let due = automation::tick_group_for_delivery_actors(&home, &group.group_id, true, &eligible)
        .expect("running actor tick");
    assert_eq!(due.notifications.len(), 1);
    assert_eq!(due.notifications[0].data["kind"], "mail_notice");
    assert_eq!(due.notifications[0].data["context"]["count"], 1);
    assert!(
        due.notifications[0].data["context"]
            .get("deliver_by")
            .is_none(),
        "a notice sent at the busy delay is not held"
    );
    assert!(
        !due.notifications[0].data["message"]
            .as_str()
            .unwrap_or_default()
            .contains("private work detail")
    );

    let repeated =
        automation::tick_group_for_delivery_actors(&home, &group.group_id, true, &eligible)
            .expect("repeated tick");
    assert!(repeated.notifications.is_empty());
}

#[test]
fn idle_actors_get_waiting_mail_promptly_while_busy_actors_are_not_interrupted() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    let store = GroupStore::new(home.clone()).expect("store");
    let group = store.create("mail backpressure", "").expect("group");
    let configure = |delivery: serde_json::Value| {
        store
            .mutate(&group.group_id, |group| {
                group.extra.insert("delivery".into(), delivery.clone());
                Ok(())
            })
            .expect("delivery settings");
    };
    store
        .mutate(&group.group_id, |group| {
            group.state = GroupState::Active;
            actors::add(group, Actor::new("idle"))?;
            actors::add(group, Actor::new("busy"))?;
            Ok(())
        })
        .expect("actors");
    configure(json!({"mail_notice_after_seconds":3600}));
    // Two minutes ago: past the default 60s idle delay, far from the hour.
    let sent_at = (chrono::Utc::now() - chrono::Duration::seconds(120)).to_rfc3339();
    for recipient in ["idle", "busy"] {
        let mut message = Event::new("chat.message", &group.group_id);
        message.by = "user".into();
        message.ts = sent_at.clone();
        message.data = json!({"text":"review this","to":[recipient],"message_mode":"mail"})
            .as_object()
            .cloned()
            .expect("message");
        ledger::append(
            &store.ledger_path(&group.group_id).expect("ledger"),
            &message,
        )
        .expect("append Mail");
    }
    let eligible = HashSet::from(["idle".to_owned(), "busy".to_owned()]);
    let idle = HashSet::from(["idle".to_owned()]);
    let tick = |idle: &HashSet<String>| {
        automation::tick_group_with_idle_actors(&home, &group.group_id, true, &eligible, idle)
            .expect("tick")
    };

    // An idle-delay window not yet over keeps even an idle actor waiting.
    configure(json!({"mail_notice_after_seconds":3600,"mail_notice_idle_after_seconds":600}));
    assert!(tick(&idle).notifications.is_empty());

    // Notices stay off entirely when the group disables them.
    configure(json!({"mail_notice_after_seconds":0}));
    assert!(tick(&idle).notifications.is_empty());

    configure(json!({"mail_notice_after_seconds":3600}));
    let due = tick(&idle);
    assert_eq!(
        due.notifications.len(),
        1,
        "only the idle actor is told now"
    );
    assert_eq!(due.notifications[0].data["kind"], "mail_notice");
    assert_eq!(due.notifications[0].data["context"]["actor_id"], "idle");
    // An early notice carries the time the busy delay would have sent it, so
    // delivery can hold it for a busy Actor but never beyond that.
    let deliver_by = chrono::DateTime::parse_from_rfc3339(
        due.notifications[0].data["context"]["deliver_by"]
            .as_str()
            .expect("deliver_by"),
    )
    .expect("rfc3339");
    let sent = chrono::DateTime::parse_from_rfc3339(&sent_at).expect("sent_at");
    assert_eq!(deliver_by.timestamp(), sent.timestamp() + 3600);

    // The working actor's Mail still waits for the full delay, and the idle
    // actor's notice stays one-shot once sent.
    assert!(tick(&idle).notifications.is_empty());
    assert!(tick(&HashSet::new()).notifications.is_empty());
}

#[test]
fn actor_start_begins_a_fresh_mail_notice_window() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    let store = GroupStore::new(home.clone()).expect("store");
    let group = store.create("actor resume window", "").expect("group");
    store
        .mutate(&group.group_id, |group| {
            group.state = GroupState::Active;
            actors::add(group, Actor::new("peer"))?;
            group
                .extra
                .insert("delivery".into(), json!({"mail_notice_after_seconds":60}));
            Ok(())
        })
        .expect("delivery settings");
    let ledger_path = store.ledger_path(&group.group_id).expect("ledger");
    let mut message = Event::new("chat.message", &group.group_id);
    message.by = "user".into();
    message.ts = "2020-01-01T00:00:00Z".into();
    message.data = json!({
        "text":"old Mail","to":["peer"],"message_mode":"mail"
    })
    .as_object()
    .cloned()
    .expect("message");
    ledger::append(&ledger_path, &message).expect("append Mail");
    let mut started = Event::new("actor.start", &group.group_id);
    started.by = "user".into();
    started.data = json!({"actor_id":"peer","runner":"headless"})
        .as_object()
        .cloned()
        .expect("start data");
    ledger::append(&ledger_path, &started).expect("append actor start");

    let eligible = HashSet::from(["peer".to_owned()]);
    let tick = automation::tick_group_for_delivery_actors(&home, &group.group_id, true, &eligible)
        .expect("post-start tick");
    assert!(
        tick.notifications.is_empty(),
        "old Mail must wait for a fresh notice window after actor.start"
    );
}

#[test]
fn mail_arriving_before_batch_closure_shares_the_existing_notice() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    let store = GroupStore::new(home.clone()).expect("store");
    let group = store.create("evolving mail batch", "").expect("group");
    store
        .mutate(&group.group_id, |group| {
            group.state = GroupState::Active;
            actors::add(group, Actor::new("peer"))?;
            group
                .extra
                .insert("delivery".into(), json!({"mail_notice_after_seconds":1}));
            Ok(())
        })
        .expect("delivery settings");
    let ledger_path = store.ledger_path(&group.group_id).expect("ledger");
    let append_mail = |text: &str| {
        let mut event = Event::new("chat.message", &group.group_id);
        event.by = "user".into();
        event.ts = "2020-01-01T00:00:00Z".into();
        event.data = json!({"text":text,"to":["peer"],"message_mode":"mail"})
            .as_object()
            .cloned()
            .expect("mail data");
        ledger::append(&ledger_path, &event).expect("append mail");
        event
    };
    let append_reply = |source: &Event| {
        let mut event = Event::new("chat.message", &group.group_id);
        event.by = "peer".into();
        event.data = json!({
            "text":"handled","to":["user"],"message_mode":"send","reply_to":source.id
        })
        .as_object()
        .cloned()
        .expect("reply data");
        ledger::append(&ledger_path, &event).expect("append reply");
    };
    let eligible = HashSet::from(["peer".to_owned()]);

    let first = append_mail("first batch item");
    let initial =
        automation::tick_group_for_delivery_actors(&home, &group.group_id, true, &eligible)
            .expect("initial notice");
    assert_eq!(initial.notifications.len(), 1);

    let joined = append_mail("joined before closure");
    append_reply(&first);
    let same_batch =
        automation::tick_group_for_delivery_actors(&home, &group.group_id, true, &eligible)
            .expect("same batch tick");
    assert!(
        same_batch.notifications.is_empty(),
        "Mail that arrived before the original batch closed must not create another prompt"
    );

    append_reply(&joined);
    let next = append_mail("next batch item");
    let next_batch =
        automation::tick_group_for_delivery_actors(&home, &group.group_id, true, &eligible)
            .expect("next batch notice");
    assert_eq!(next_batch.notifications.len(), 1);
    assert_eq!(
        next_batch.notifications[0].data["context"]["source_event_ids"],
        json!([next.id])
    );
}

#[test]
fn reply_notice_starts_only_after_delivery_acceptance() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    let store = GroupStore::new(home.clone()).expect("store");
    let group = store.create("automation precedence", "").expect("group");
    store
        .mutate(&group.group_id, |group| {
            group.state = GroupState::Active;
            actors::add(group, Actor::new("peer"))?;
            group
                .extra
                .insert("delivery".into(), json!({"reply_notice_after_seconds":1}));
            Ok(())
        })
        .expect("automation config");
    let mut message = Event::new("chat.message", &group.group_id);
    message.by = "user".into();
    message.ts = "2020-01-01T00:00:00Z".into();
    message.data = json!({
        "text":"please answer",
        "to":["peer"],
        "message_mode":"request_reply"
    })
    .as_object()
    .cloned()
    .expect("message");
    ledger::append(
        &store.ledger_path(&group.group_id).expect("ledger"),
        &message,
    )
    .expect("append unread message");

    let eligible = HashSet::from(["peer".to_owned()]);
    let before_acceptance =
        automation::tick_group_for_delivery_actors(&home, &group.group_id, true, &eligible)
            .expect("pre-acceptance tick");
    assert!(before_acceptance.notifications.is_empty());

    let mut accepted = Event::new("runtime.delivery", &group.group_id);
    accepted.by = "system".into();
    accepted.ts = "2020-01-01T00:00:01Z".into();
    accepted.data = json!({
        "source_event_id":message.id,
        "actor_id":"peer",
        "state":"accepted"
    })
    .as_object()
    .cloned()
    .expect("delivery fact");
    ledger::append(
        &store.ledger_path(&group.group_id).expect("ledger"),
        &accepted,
    )
    .expect("append accepted delivery");

    let due = automation::tick_group_for_delivery_actors(&home, &group.group_id, true, &eligible)
        .expect("reply notice tick");
    assert_eq!(due.notifications.len(), 1);
    assert_eq!(due.notifications[0].data["kind"], "reply_notice");
    let repeated =
        automation::tick_group_for_delivery_actors(&home, &group.group_id, true, &eligible)
            .expect("repeated tick");
    assert!(repeated.notifications.is_empty());
}

#[test]
fn scheduled_action_remains_due_until_its_owner_confirms_completion() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    let store = GroupStore::new(home.clone()).expect("store");
    let group = store.create("automation action", "").expect("group");
    store
        .mutate(&group.group_id, |group| {
            group.automation = json!({
                "version":1,
                "rules":[{
                    "id":"pause-once","enabled":true,"scope":"group",
                    "trigger":{"kind":"at","at":"2020-01-01T00:00:00Z"},
                    "action":{"kind":"group_state","state":"paused"}
                }]
            })
            .as_object()
            .cloned()
            .expect("automation");
            Ok(())
        })
        .expect("automation rule");

    let first = automation::tick_group(&home, &group.group_id, false).expect("first tick");
    assert_eq!(first.actions.len(), 1);
    let unconfirmed =
        automation::tick_group(&home, &group.group_id, false).expect("unconfirmed tick");
    assert_eq!(
        unconfirmed.actions.len(),
        1,
        "returning an action is not proof that the daemon applied it"
    );
}

/// A managed session that reports idle, holds active cards, and has been
/// quiet for the interval, is reminded about them once per interval.
#[test]
fn card_wake_reminds_an_idle_actor_about_its_active_cards_once_per_interval() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    let store = GroupStore::new(home.clone()).expect("store");
    let group = store.create("card wake", "").expect("group");
    let configure = |delivery: serde_json::Value| {
        store
            .mutate(&group.group_id, |group| {
                group.extra.insert("delivery".into(), delivery.clone());
                Ok(())
            })
            .expect("delivery settings");
    };
    store
        .mutate(&group.group_id, |group| {
            group.state = GroupState::Active;
            actors::add(group, Actor::new("peer"))?;
            Ok(())
        })
        .expect("actor");
    configure(json!({"task_wake_on_idle":true,"task_wake_interval_seconds":600}));

    let create_card = |fields: serde_json::Value| {
        let mut op = json!({"op":"task.create","title":"PRIVATE_CARD_TITLE"})
            .as_object()
            .cloned()
            .expect("card op");
        op.extend(fields.as_object().cloned().expect("card fields"));
        cccc_core::context::ContextStore::new(home.clone())
            .expect("contexts")
            .sync(&group.group_id, &[op], None, "user", false)
            .expect("create card");
    };
    create_card(json!({"status":"active","assignee":"peer"}));

    let eligible = HashSet::from(["peer".to_owned()]);
    let idle = HashSet::from(["peer".to_owned()]);
    let tick = |idle: &HashSet<String>| {
        automation::tick_group_with_idle_actors(&home, &group.group_id, true, &eligible, idle)
            .expect("tick")
    };
    fn card_notices(result: &cccc_core::automation::TickResult) -> Vec<&Event> {
        result
            .notifications
            .iter()
            .filter(|event| event.data["kind"] == "task_notice")
            .collect()
    }

    let due = tick(&idle);
    let notices = card_notices(&due);
    assert_eq!(notices.len(), 1, "{:?}", due.notifications);
    assert_eq!(notices[0].data["target_actor_id"], "peer");
    assert_eq!(notices[0].by, "system");
    assert_eq!(notices[0].data["im_visibility"], "internal");
    // The notice names card ids and nothing else: no title, no task content.
    let ids = notices[0].data["context"]["task_ids"]
        .as_array()
        .expect("task_ids");
    assert_eq!(ids.len(), 1);
    let rendered = notices[0].data["message"].as_str().unwrap_or_default();
    assert!(rendered.contains(ids[0].as_str().unwrap_or_default()));
    assert!(
        !rendered.contains("PRIVATE_CARD_TITLE"),
        "card content leaked into the notice: {rendered}"
    );

    // One-shot: a second tick in the same window stays silent.
    assert!(card_notices(&tick(&idle)).is_empty());

    // A busy actor is never prompted.
    assert!(card_notices(&tick(&HashSet::new())).is_empty());
}

#[test]
fn card_wake_is_off_unless_the_group_opts_in() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    let store = GroupStore::new(home.clone()).expect("store");
    let group = store.create("card wake off", "").expect("group");
    store
        .mutate(&group.group_id, |group| {
            group.state = GroupState::Active;
            actors::add(group, Actor::new("peer"))?;
            Ok(())
        })
        .expect("actor");
    let op = json!({"op":"task.create","title":"t","status":"active","assignee":"peer"})
        .as_object()
        .cloned()
        .expect("card op");
    cccc_core::context::ContextStore::new(home.clone())
        .expect("contexts")
        .sync(&group.group_id, &[op], None, "user", false)
        .expect("create card");

    let eligible = HashSet::from(["peer".to_owned()]);
    let idle = HashSet::from(["peer".to_owned()]);
    let result = automation::tick_group_with_idle_actors(
        &home,
        &group.group_id,
        true,
        &eligible,
        &idle,
    )
    .expect("tick");
    assert!(
        !result
            .notifications
            .iter()
            .any(|event| event.data["kind"] == "task_notice"),
        "card notice without the opt-in"
    );
}

#[test]
fn card_wake_skips_cards_that_are_not_the_actors_to_move() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    let store = GroupStore::new(home.clone()).expect("store");
    let group = store.create("card wake filter", "").expect("group");
    store
        .mutate(&group.group_id, |group| {
            group.state = GroupState::Active;
            actors::add(group, Actor::new("peer"))?;
            group.extra.insert("delivery".into(), json!({"task_wake_on_idle":true}));
            Ok(())
        })
        .expect("actor");
    let card = |fields: serde_json::Value| {
        let mut op = json!({"op":"task.create","title":"t"})
            .as_object()
            .cloned()
            .expect("card op");
        op.extend(fields.as_object().cloned().expect("fields"));
        cccc_core::context::ContextStore::new(home.clone())
            .expect("contexts")
            .sync(&group.group_id, &[op], None, "user", false)
            .expect("create card");
    };
    // None of these are work this actor should be reminded to move: not
    // active, assigned elsewhere, blocked, or waiting on the user.
    card(json!({"status":"planned","assignee":"peer"}));
    card(json!({"status":"active","assignee":"other"}));
    card(json!({"status":"active","assignee":"peer","blocked_by":["T1"]}));
    card(json!({"status":"active","assignee":"peer","waiting_on":"user"}));

    let eligible = HashSet::from(["peer".to_owned()]);
    let result = automation::tick_group_with_idle_actors(
        &home,
        &group.group_id,
        true,
        &eligible,
        &HashSet::from(["peer".to_owned()]),
    )
    .expect("tick");
    assert!(
        !result
            .notifications
            .iter()
            .any(|event| event.data["kind"] == "task_notice"),
        "reminded about a card that is not this actor's to move"
    );
}

#[test]
fn card_wake_waits_for_the_quiet_interval_before_reminding() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    let store = GroupStore::new(home.clone()).expect("store");
    let group = store.create("card wake interval", "").expect("group");
    store
        .mutate(&group.group_id, |group| {
            group.state = GroupState::Active;
            actors::add(group, Actor::new("peer"))?;
            group.extra.insert(
                "delivery".into(),
                json!({"task_wake_on_idle":true,"task_wake_interval_seconds":3600}),
            );
            Ok(())
        })
        .expect("actor");
    let op = json!({"op":"task.create","title":"t","status":"active","assignee":"peer"})
        .as_object()
        .cloned()
        .expect("card op");
    cccc_core::context::ContextStore::new(home.clone())
        .expect("contexts")
        .sync(&group.group_id, &[op], None, "user", false)
        .expect("create card");

    // A message from the actor two minutes ago closes its quiet window.
    let mut message = Event::new("chat.message", &group.group_id);
    message.by = "peer".into();
    message.ts = (chrono::Utc::now() - chrono::Duration::seconds(120)).to_rfc3339();
    message.data = json!({"text":"working","to":["user"]})
        .as_object()
        .cloned()
        .expect("message");
    ledger::append(
        &store.ledger_path(&group.group_id).expect("ledger"),
        &message,
    )
    .expect("append");

    let eligible = HashSet::from(["peer".to_owned()]);
    let idle = HashSet::from(["peer".to_owned()]);
    let result = automation::tick_group_with_idle_actors(&home, &group.group_id, true, &eligible, &idle)
        .expect("tick");
    assert!(
        !result
            .notifications
            .iter()
            .any(|event| event.data["kind"] == "task_notice"),
        "reminded inside the quiet window"
    );
}
