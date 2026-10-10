// Included by the crate-level integration test harness.
use cccc_contracts::{Actor, Event, GroupState};
use cccc_core::{GroupStore, HomeLayout, actors, automation, ledger};
use serde_json::json;
use std::collections::{HashMap, HashSet};

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

struct MailWake {
    _temp: tempfile::TempDir,
    home: HomeLayout,
    store: GroupStore,
    group_id: String,
}

impl MailWake {
    fn new(delivery: serde_json::Value) -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let group = store.create("mail wake", "").expect("group");
        store
            .mutate(&group.group_id, |group| {
                group.state = GroupState::Active;
                actors::add(group, Actor::new("peer"))?;
                group.extra.insert("delivery".into(), delivery);
                Ok(())
            })
            .expect("delivery settings");
        Self {
            _temp: temp,
            home,
            store,
            group_id: group.group_id,
        }
    }

    fn append(&self, event: &Event) {
        let path = self.store.ledger_path(&self.group_id).expect("ledger");
        ledger::append(&path, event).expect("append");
    }

    fn mail(&self, age_seconds: i64, to: serde_json::Value) -> Event {
        let mut event = Event::new("chat.message", &self.group_id);
        event.by = "user".into();
        event.ts = (chrono::Utc::now() - chrono::Duration::seconds(age_seconds)).to_rfc3339();
        event.data = json!({"text":"PRIVATE_MAIL_BODY","to":to,"message_mode":"mail"})
            .as_object()
            .cloned()
            .expect("mail data");
        self.append(&event);
        event
    }

    /// `managed`: `Some(idle)` for a running managed session, `None` for PTY.
    fn tick(&self, managed: Option<bool>) -> Vec<Event> {
        let managed_idle = managed
            .map(|idle| HashMap::from([("peer".to_owned(), idle)]))
            .unwrap_or_default();
        automation::tick_group_for_delivery_actors_with_managed_idle(
            &self.home,
            &self.group_id,
            true,
            &HashSet::from(["peer".to_owned()]),
            &managed_idle,
        )
        .expect("tick")
        .notifications
    }

    fn accept(&self, notice: &Event) {
        let mut event = Event::new("runtime.delivery", &self.group_id);
        event.by = "system".into();
        event.data = json!({"source_event_id":notice.id,"actor_id":"peer","state":"accepted"})
            .as_object()
            .cloned()
            .expect("delivery data");
        self.append(&event);
    }

    fn mutate(&self, change: impl FnOnce(&mut cccc_core::GroupDoc)) {
        self.store
            .mutate(&self.group_id, |group| {
                change(group);
                Ok(())
            })
            .expect("mutate group");
    }

    fn group(&self) -> cccc_core::GroupDoc {
        self.store.load(&self.group_id).expect("group")
    }
}

#[test]
fn mail_wake_off_keeps_the_timer_and_batch_latch_for_managed_sessions() {
    for delivery in [
        json!({"mail_notice_after_seconds":1800}),
        json!({"mail_notice_after_seconds":1800,"mail_wake_on_idle":false}),
    ] {
        let young = MailWake::new(delivery.clone());
        young.mail(120, json!(["peer"]));
        assert!(
            young.tick(Some(true)).is_empty(),
            "young Mail waits for the timer"
        );
        let fixture = MailWake::new(delivery);
        fixture.mail(7_200, json!(["peer"]));
        let first = fixture.tick(Some(true));
        assert_eq!(first.len(), 1, "the timer notice is unchanged");
        fixture.accept(&first[0]);
        fixture.mail(7_200, json!(["peer"]));
        assert!(
            fixture.tick(Some(true)).is_empty(),
            "without the opt-in, an unresolved batch still holds later Mail"
        );
    }
}

#[test]
fn mail_wake_notifies_an_idle_managed_session_once_without_reading_mail() {
    let fixture = MailWake::new(json!({"mail_wake_on_idle":true}));
    let mail = fixture.mail(120, json!(["peer"]));
    let notices = fixture.tick(Some(true));
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].data["kind"], "mail_notice");
    assert_eq!(
        notices[0].data["context"]["source_event_ids"],
        json!([mail.id])
    );
    assert!(
        !serde_json::to_string(&notices[0])
            .expect("notice json")
            .contains("PRIVATE_MAIL_BODY")
    );
    assert!(
        fixture.tick(Some(true)).is_empty(),
        "a notified batch never repeats"
    );
    let group = fixture.group();
    assert_eq!(
        cccc_core::inbox::list_unread(&fixture.home, &group, "peer", 10)
            .expect("unread")
            .len(),
        1
    );
    assert!(
        cccc_core::inbox::cursors(&fixture.home, &fixture.group_id)
            .expect("cursors")
            .is_empty(),
        "the notice never advances the Mail cursor"
    );
}

#[test]
fn mail_wake_waits_for_a_busy_managed_session_to_end_its_turn() {
    let fixture = MailWake::new(json!({"mail_wake_on_idle":true}));
    fixture.mail(7_200, json!(["peer"]));
    assert!(
        fixture.tick(Some(false)).is_empty(),
        "a working session is not interrupted, even past the timer"
    );
    assert_eq!(fixture.tick(Some(true)).len(), 1);
}

#[test]
fn mail_wake_notifies_new_mail_after_an_ignored_notice() {
    let fixture = MailWake::new(json!({"mail_wake_on_idle":true}));
    fixture.mail(120, json!(["peer"]));
    let first = fixture.tick(Some(true));
    assert_eq!(first.len(), 1);
    fixture.accept(&first[0]);
    assert!(fixture.tick(Some(true)).is_empty());

    let second = fixture.mail(120, json!(["peer"]));
    let notices = fixture.tick(Some(true));
    assert_eq!(
        notices.len(),
        1,
        "later Mail is not held by the ignored batch"
    );
    assert_eq!(
        notices[0].data["context"]["source_event_ids"],
        json!([second.id])
    );
    assert!(fixture.tick(Some(true)).is_empty());
}

#[test]
fn mail_wake_leaves_pty_actors_on_the_timer_and_latch() {
    let young = MailWake::new(json!({"mail_wake_on_idle":true}));
    young.mail(120, json!(["peer"]));
    assert!(
        young.tick(None).is_empty(),
        "young Mail waits for the timer"
    );
    let fixture = MailWake::new(json!({"mail_wake_on_idle":true}));
    fixture.mail(7_200, json!(["peer"]));
    let first = fixture.tick(None);
    assert_eq!(first.len(), 1);
    fixture.accept(&first[0]);
    fixture.mail(7_200, json!(["peer"]));
    assert!(fixture.tick(None).is_empty());
}

#[test]
fn mail_wake_respects_minimum_age_and_excludes_broadcast_mail() {
    for to in [
        json!(["@all"]),
        json!(["@peers"]),
        json!(["@foreman"]),
        json!([]),
    ] {
        let fixture = MailWake::new(json!({"mail_wake_on_idle":true}));
        fixture.mail(7_200, to.clone());
        assert!(fixture.tick(Some(true)).is_empty(), "{to}");
    }
    let fixture = MailWake::new(json!({"mail_wake_on_idle":true}));
    fixture.mail(30, json!(["peer"]));
    assert!(
        fixture.tick(Some(true)).is_empty(),
        "default minimum age is 60s"
    );
    fixture.mutate(|group| {
        group.extra.get_mut("delivery").expect("delivery")["mail_wake_min_age_seconds"] = json!(0);
    });
    assert_eq!(fixture.tick(Some(true)).len(), 1);
}

#[test]
fn mail_wake_skips_paused_stopped_disabled_and_read_mail() {
    for state in [GroupState::Paused, GroupState::Stopped] {
        let fixture = MailWake::new(json!({"mail_wake_on_idle":true}));
        fixture.mail(120, json!(["peer"]));
        fixture.mutate(|group| group.state = state);
        assert!(fixture.tick(Some(true)).is_empty(), "{state:?}");
    }
    let fixture = MailWake::new(json!({"mail_wake_on_idle":true}));
    fixture.mail(120, json!(["peer"]));
    fixture.mutate(|group| group.actors[0].enabled = false);
    assert!(fixture.tick(Some(true)).is_empty(), "disabled");
    fixture.mutate(|group| group.actors[0].enabled = true);
    let group = fixture.group();
    cccc_core::inbox::consume_unread(&fixture.home, &group, "peer", "peer", 10).expect("read");
    assert!(fixture.tick(Some(true)).is_empty(), "read Mail");
}

impl MailWake {
    fn card(&self, fields: serde_json::Value) {
        let mut op = json!({"op":"task.create","title":"PRIVATE_CARD_TITLE"})
            .as_object()
            .cloned()
            .expect("card op");
        op.extend(fields.as_object().cloned().expect("card fields"));
        cccc_core::context::ContextStore::new(self.home.clone())
            .expect("contexts")
            .sync(&self.group_id, &[op], None, "user", false)
            .expect("create card");
    }

    fn system_event(&self, kind: &str, data: serde_json::Value, age_seconds: i64) {
        let mut event = Event::new(kind, &self.group_id);
        event.by = "system".into();
        event.ts = (chrono::Utc::now() - chrono::Duration::seconds(age_seconds)).to_rfc3339();
        event.data = data.as_object().cloned().expect("event data");
        self.append(&event);
    }
}

fn card_notices(notices: &[Event]) -> Vec<&Event> {
    notices
        .iter()
        .filter(|event| event.data["kind"] == "task_notice")
        .collect()
}

#[test]
fn task_wake_off_never_reminds_about_cards() {
    for delivery in [json!({}), json!({"task_wake_on_idle":false})] {
        let fixture = MailWake::new(delivery);
        fixture.card(json!({"status":"active","assignee":"peer"}));
        assert!(card_notices(&fixture.tick(Some(true))).is_empty());
    }
}

#[test]
fn task_wake_reminds_an_idle_managed_session_about_its_active_cards_once_per_interval() {
    let fixture = MailWake::new(json!({"task_wake_on_idle":true}));
    fixture.card(json!({"status":"active","assignee":"peer"}));
    fixture.card(json!({"status":"active","assignee":"peer"}));
    let notices = fixture.tick(Some(true));
    let cards = card_notices(&notices);
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0].data["target_actor_id"], "peer");
    assert_eq!(
        cards[0].data["context"]["task_ids"],
        json!(["T001", "T002"])
    );
    assert!(
        !serde_json::to_string(cards[0])
            .expect("notice json")
            .contains("PRIVATE_CARD_TITLE"),
        "reminders name cards, never their content"
    );
    assert!(
        card_notices(&fixture.tick(Some(true))).is_empty(),
        "at most one reminder per interval"
    );
}

#[test]
fn task_wake_reminds_again_only_after_a_quiet_interval() {
    let fixture = MailWake::new(json!({"task_wake_on_idle":true,"task_wake_interval_seconds":600}));
    fixture.card(json!({"status":"active","assignee":"peer"}));
    fixture.system_event(
        "system.notify",
        json!({"kind":"task_notice","target_actor_id":"peer"}),
        1_200,
    );
    assert_eq!(card_notices(&fixture.tick(Some(true))).len(), 1);

    let recent = MailWake::new(json!({"task_wake_on_idle":true,"task_wake_interval_seconds":600}));
    recent.card(json!({"status":"active","assignee":"peer"}));
    let mut message = Event::new("chat.message", &recent.group_id);
    message.by = "peer".into();
    message.data = json!({"text":"working on it","to":["user"],"message_mode":"send"})
        .as_object()
        .cloned()
        .expect("message");
    recent.append(&message);
    assert!(
        card_notices(&recent.tick(Some(true))).is_empty(),
        "a lane that spoke within the interval is not prompted"
    );

    let restarted = MailWake::new(json!({"task_wake_on_idle":true}));
    restarted.card(json!({"status":"active","assignee":"peer"}));
    restarted.system_event("actor.start", json!({"actor_id":"peer"}), 30);
    assert!(
        card_notices(&restarted.tick(Some(true))).is_empty(),
        "a fresh session already sees its cards at bootstrap"
    );
}

#[test]
fn task_wake_waits_for_idle_and_ignores_pty_actors() {
    let fixture = MailWake::new(json!({"task_wake_on_idle":true}));
    fixture.card(json!({"status":"active","assignee":"peer"}));
    assert!(
        card_notices(&fixture.tick(Some(false))).is_empty(),
        "working"
    );
    assert!(card_notices(&fixture.tick(None)).is_empty(), "PTY");
    assert_eq!(card_notices(&fixture.tick(Some(true))).len(), 1);
}

#[test]
fn task_wake_skips_cards_that_are_not_the_actors_to_move() {
    for fields in [
        json!({"status":"planned","assignee":"peer"}),
        json!({"status":"done","assignee":"peer"}),
        json!({"status":"active"}),
        json!({"status":"active","assignee":"other"}),
        json!({"status":"active","assignee":"peer","blocked_by":["T009"]}),
        json!({"status":"active","assignee":"peer","waiting_on":"user"}),
        json!({"status":"active","assignee":"peer","waiting_on":"actor"}),
        json!({"status":"active","assignee":"peer","waiting_on":"external"}),
    ] {
        let fixture = MailWake::new(json!({"task_wake_on_idle":true}));
        fixture.card(fields.clone());
        assert!(
            card_notices(&fixture.tick(Some(true))).is_empty(),
            "{fields}"
        );
    }
}
