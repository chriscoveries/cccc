use super::*;
use crate::actors;

struct Fixture {
    _temp: tempfile::TempDir,
    home: HomeLayout,
    group: GroupDoc,
    actor: Actor,
    now: i64,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("temp");
        let home = HomeLayout::from_path(temp.path()).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let mut group = store.create("attention", "").expect("group");
        group.state = GroupState::Active;
        group.running = true;
        let actor = Actor::new("peer");
        actors::add(&mut group, actor.clone()).expect("actor");
        store.save(&group).expect("save");
        Self {
            _temp: temp,
            home,
            group,
            actor,
            now: 1_790_000_000,
        }
    }
    fn event(&self, mut event: Event) -> String {
        if event.ts == "" {
            event.ts = self.ts(self.now);
        }
        let id = event.id.clone();
        ledger::append(&self.path(), &event).expect("append");
        id
    }
    fn path(&self) -> PathBuf {
        GroupStore::new(self.home.clone())
            .expect("store")
            .ledger_path(&self.group.group_id)
            .expect("path")
    }
    fn ts(&self, at: i64) -> String {
        DateTime::<Utc>::from_timestamp(at, 0)
            .expect("clock")
            .to_rfc3339()
    }
    fn mail(&self, at: i64, to: &[&str]) -> String {
        let mut e = Event::new("chat.message", &self.group.group_id);
        e.by = "sender".into();
        e.ts = self.ts(at);
        e.data = json!({"message_mode":"mail","to":to,"text":"private source body"})
            .as_object()
            .cloned()
            .expect("data");
        self.event(e)
    }
    fn offer(&self, carrier: &str, now: i64) -> Option<Hint> {
        offer_context(
            &self.home,
            &self.group.group_id,
            &self.actor.id,
            carrier,
            now,
        )
        .expect("offer")
    }
    fn summary(&self, now: i64) -> Summary {
        inspect(&self.home, &self.group.group_id, &self.actor.id, now).expect("inspect")
    }
    fn read(&self, boundary: &str, at: i64) {
        let mut e = Event::new("mail.read", &self.group.group_id);
        e.by = self.actor.id.clone();
        e.ts = self.ts(at);
        e.data = json!({"actor_id":self.actor.id,"event_id":boundary})
            .as_object()
            .cloned()
            .expect("data");
        self.event(e);
        let store = GroupStore::new(self.home.clone()).expect("store");
        fs::write_json(&store.group_dir(&self.group.group_id).expect("dir").join("state/read_cursors.json"),
            &json!({"schema":1,"cursors":{self.actor.id.clone():{"event_id":boundary,"ts":self.ts(at),"updated_at":self.ts(at)}}})).expect("cursor");
    }
}

#[test]
fn empty_and_busy_scans_are_silent_and_never_make_a_notify() {
    let f = Fixture::new();
    for _ in 0..3 {
        scan(&f.home, &f.group.group_id, f.now).expect("scan");
    }
    assert!(ledger::read_all(&f.path()).expect("events").is_empty());
    f.mail(f.now, &["peer"]);
    scan(&f.home, &f.group.group_id, f.now + 301).expect("scan");
    let first = ledger::read_all(&f.path()).expect("events");
    for _ in 0..4 {
        scan(&f.home, &f.group.group_id, f.now + 1000).expect("busy scan");
    }
    assert_eq!(first, ledger::read_all(&f.path()).expect("events"));
    assert!(!first.iter().any(|e| e.kind == "system.notify"));
    assert_eq!(f.summary(f.now + 1000).state.attempt_no, 0);
}

#[test]
fn context_paths_share_one_due_token_and_retry_carriers_never_repeat() {
    let f = Fixture::new();
    f.mail(f.now, &["peer"]);
    assert!(f.offer("bootstrap", f.now + 299).is_none());
    let hint = f.offer("bootstrap", f.now + 300).expect("due");
    assert_eq!(hint.attention_count, 1);
    assert!(f.offer("mcp-message", f.now + 300).is_none());
    let due = f.summary(f.now + 300).state.due_at;
    assert!((f.now + 900..=f.now + 930).contains(&due));
    assert!(
        f.offer("bootstrap", due + 1).is_none(),
        "lost/retried carrier is not offered again"
    );
    assert!(f.offer("mcp-message", due).is_some());
    assert_eq!(f.summary(due).state.standalone_wakeups, 0);
    assert_eq!(
        inbox::cursor(&f.home, &f.group.group_id, &f.actor.id).expect("cursor"),
        None
    );
}

#[test]
fn partial_reads_and_new_arrivals_do_not_reset_backoff_or_episode() {
    let f = Fixture::new();
    let one = f.mail(f.now, &["peer"]);
    f.mail(f.now + 1, &["peer"]);
    f.offer("mcp1", f.now + 301).expect("hint");
    let before = f.summary(f.now + 301).state;
    f.read(&one, f.now + 302);
    f.mail(f.now + 303, &["peer"]);
    assert!(f.offer("mcp2", f.now + 304).is_none());
    let after = f.summary(f.now + 304).state;
    assert_eq!(after.episode_id, before.episode_id);
    assert_eq!(after.attempt_no, 1);
    assert_eq!(after.due_at, before.due_at);
    assert_eq!(after.attention_count, 2);
}

#[test]
fn complete_read_between_scans_closes_episode_but_preserves_minimum_cooldown() {
    let f = Fixture::new();
    let one = f.mail(f.now, &["peer"]);
    f.offer("mcp1", f.now + 300).expect("hint");
    let before = f.summary(f.now + 300).state;
    f.read(&one, f.now + 301);
    f.mail(f.now + 302, &["peer"]);
    assert!(f.offer("mcp2", f.now + 303).is_none());
    let after = f.summary(f.now + 303).state;
    assert_ne!(after.episode_id, before.episode_id);
    assert_eq!(after.attempt_no, 0);
    assert!(after.due_at >= f.now + 600);
}

#[test]
fn three_boundary_wakeups_exhaust_budget_but_passive_hints_remain() {
    let f = Fixture::new();
    f.mail(f.now, &["peer"]);
    let mut now = f.now + 300;
    for i in 0..3 {
        let source = format!("turn-{i}");
        assert!(
            reserve_boundary_turn(&f.home, &f.group.group_id, &f.actor.id, &source, now)
                .expect("reserve")
                .is_some()
        );
        finish_carrier(
            &f.home,
            &f.group.group_id,
            &f.actor.id,
            &source,
            "accepted",
            now,
        )
        .expect("finish");
        now = f.summary(now).state.due_at;
    }
    assert_eq!(f.summary(now).state.standalone_wakeups, 3);
    assert!(
        reserve_boundary_turn(&f.home, &f.group.group_id, &f.actor.id, "fourth", now)
            .expect("reserve")
            .is_none()
    );
    assert!(f.offer("ordinary-work", now).is_some());
    assert_eq!(f.summary(now).state.standalone_wakeups, 3);
}

#[test]
fn uncertain_reservation_on_restart_is_not_replayed_and_cache_loss_preserves_budget() {
    let f = Fixture::new();
    f.mail(f.now, &["peer"]);
    transact(
        &f.home,
        &f.group.group_id,
        f.now + 300,
        "old-process",
        |tx| tx.present("peer", &["native-turn".into()], true, false),
    )
    .expect("reserve");
    let cache = GroupStore::new(f.home.clone())
        .expect("store")
        .group_dir(&f.group.group_id)
        .expect("dir")
        .join("state/mail-attention.json");
    std::fs::remove_file(&cache).expect("cache deletion");
    scan(&f.home, &f.group.group_id, f.now + 301).expect("replay");
    let s = f.summary(f.now + 301).state;
    assert_eq!(s.standalone_wakeups, 1);
    assert_eq!(s.attempt_no, 1);
    assert!(s.pending.is_none());
    assert!(
        reserve_boundary_turn(
            &f.home,
            &f.group.group_id,
            &f.actor.id,
            "native-turn",
            s.due_at
        )
        .expect("reserve")
        .is_none()
    );
    std::fs::write(&cache, b"broken-cache").expect("corrupt cache");
    scan(&f.home, &f.group.group_id, f.now + 302).expect("reconstruct");
    assert_eq!(f.summary(f.now + 302).state.standalone_wakeups, 1);
}

#[test]
fn expiry_is_exact_and_rollback_cannot_revive_mail_or_advance_due_time() {
    let f = Fixture::new();
    f.mail(f.now - EXPIRY_SECONDS + 1, &["peer"]);
    assert!(f.offer("last-fresh", f.now).is_some());
    assert!(f.offer("expiry-boundary", f.now + 1).is_none());
    let s = f.summary(f.now + 1);
    assert_eq!(s.attention_count, 0);
    assert_eq!(s.unread_count, 1);
    assert_eq!(s.expired_unread_count, 1);
    assert!(f.offer("rollback", f.now - 10).is_none());
    assert_eq!(f.summary(f.now - 10).attention_count, 0);
}

#[test]
fn old_broadcast_replied_promoted_invalid_future_and_self_mail_have_no_hint() {
    let f = Fixture::new();
    f.mail(f.now - EXPIRY_SECONDS, &["peer"]);
    f.mail(f.now, &["@all"]);
    f.mail(f.now + 100, &["peer"]);
    let replysource = f.mail(f.now - 10, &["peer"]);
    let mut reply = Event::new("chat.message", &f.group.group_id);
    reply.by = "peer".into();
    reply.ts = f.ts(f.now);
    reply.data = json!({"reply_to":replysource,"message_mode":"mail","to":["peer"]})
        .as_object()
        .cloned()
        .expect("data");
    f.event(reply);
    let promoted = f.mail(f.now - 10, &["peer"]);
    let mut delivery = Event::new("runtime.delivery", &f.group.group_id);
    delivery.ts = f.ts(f.now);
    delivery.data = json!({"actor_id":"peer","source_event_id":promoted,"state":"ambiguous"})
        .as_object()
        .cloned()
        .expect("data");
    f.event(delivery);
    let mut invalid = Event::new("chat.message", &f.group.group_id);
    invalid.by = "sender".into();
    invalid.ts = "invalid".into();
    invalid.data = json!({"message_mode":"mail","to":["peer"]})
        .as_object()
        .cloned()
        .expect("data");
    f.event(invalid);
    assert!(f.offer("response", f.now).is_none());
    let s = f.summary(f.now);
    assert_eq!(s.attention_count, 0);
    assert_eq!(s.expired_unread_count, 1);
    assert_eq!(s.invalid_unread_count, 2);
    assert_eq!(s.unread_count, 6);
}

#[test]
fn two_racing_carriers_reserve_once_and_a_batch_resolves_once() {
    let f = Fixture::new();
    f.mail(f.now, &["peer"]);
    let mut threads = Vec::new();
    for i in 0..2 {
        let home = f.home.clone();
        let gid = f.group.group_id.clone();
        let now = f.now + 300;
        threads.push(std::thread::spawn(move || {
            reserve_delivery_hint(&home, &gid, "peer", &[format!("carrier{i}")], now).expect("race")
        }));
    }
    let hints = threads
        .into_iter()
        .map(|t| t.join().expect("join"))
        .collect::<Vec<_>>();
    assert_eq!(hints.iter().filter(|h| h.is_some()).count(), 1);
    let state = f.summary(f.now + 300).state;
    let carrier = &state.pending.as_ref().expect("pending").carrier_ids[0];
    finish_carrier(
        &f.home,
        &f.group.group_id,
        "peer",
        carrier,
        "accepted",
        f.now + 301,
    )
    .expect("finish");
    finish_carrier(
        &f.home,
        &f.group.group_id,
        "peer",
        carrier,
        "accepted",
        f.now + 301,
    )
    .expect("repeat");
    assert_eq!(f.summary(f.now + 301).state.attempt_no, 1);
}

#[test]
fn malformed_journal_fails_closed_and_initial_override_zero_only_disables_standalone() {
    let f = Fixture::new();
    f.mail(f.now, &["peer"]);
    let store = GroupStore::new(f.home.clone()).expect("store");
    store
        .mutate(&f.group.group_id, |g| {
            g.extra
                .insert("delivery".into(), json!({"mail_notice_after_seconds":0}));
            Ok(())
        })
        .expect("override");
    assert!(
        reserve_boundary_turn(
            &f.home,
            &f.group.group_id,
            "peer",
            "background",
            f.now + 300
        )
        .expect("reserve")
        .is_none()
    );
    assert!(f.offer("context", f.now + 300).is_some());
    let mut malformed = Event::new("mail.attention", &f.group.group_id);
    malformed.data = json!({"version":1,"action":"token_reserved"})
        .as_object()
        .cloned()
        .expect("data");
    f.event(malformed);
    assert!(offer_context(&f.home, &f.group.group_id, "peer", "other", f.now + 10000).is_err());
}

#[test]
fn shared_delivery_ids_do_not_deduplicate_across_actors() {
    let f = Fixture::new();
    let store = GroupStore::new(f.home.clone()).expect("store");
    store
        .mutate(&f.group.group_id, |g| actors::add(g, Actor::new("other")))
        .expect("actor");
    f.mail(f.now, &["peer", "other"]);
    assert!(f.offer("broadcast-carrier", f.now + 300).is_some());
    assert!(
        offer_context(
            &f.home,
            &f.group.group_id,
            "other",
            "broadcast-carrier",
            f.now + 300
        )
        .expect("other")
        .is_some()
    );
}

#[test]
fn disabled_and_paused_lifecycles_preserve_budget_and_recreation_changes_identity() {
    let f = Fixture::new();
    f.mail(f.now, &["peer"]);
    f.offer("context", f.now + 300).expect("hint");
    let before = f.summary(f.now + 300).state;
    let store = GroupStore::new(f.home.clone()).expect("store");
    store
        .mutate(&f.group.group_id, |g| {
            g.actors[0].enabled = false;
            Ok(())
        })
        .expect("disable");
    assert!(f.offer("disabled", before.due_at + 1).is_none());
    store
        .mutate(&f.group.group_id, |g| {
            g.actors[0].enabled = true;
            g.state = GroupState::Paused;
            Ok(())
        })
        .expect("pause");
    assert!(f.offer("paused", before.due_at + 1).is_none());
    store
        .mutate(&f.group.group_id, |g| {
            g.state = GroupState::Active;
            Ok(())
        })
        .expect("resume");
    assert_eq!(f.summary(before.due_at).state.episode_id, before.episode_id);
    assert!(f.offer("resumed", before.due_at).is_some());
    let old = f.summary(before.due_at).state;
    store
        .mutate(&f.group.group_id, |g| {
            g.actors[0].generation = "new-incarnation".into();
            Ok(())
        })
        .expect("generation");
    let mut add = Event::new("actor.add", &f.group.group_id);
    add.ts = f.ts(before.due_at);
    add.data = json!({"actor":{"id":"peer"}})
        .as_object()
        .cloned()
        .expect("data");
    f.event(add);
    assert!(f.offer("recreated-empty", before.due_at + 1).is_none());
    f.mail(before.due_at + 2, &["peer"]);
    f.offer("new-generation", before.due_at + 302)
        .expect("hint");
    let new = f.summary(before.due_at + 302).state;
    assert_ne!(new.incarnation, old.incarnation);
    assert_eq!(new.attempt_no, 1);
}

#[test]
fn exact_legacy_rule_retirement_retains_definition_and_leaves_other_automation() {
    let mut f = Fixture::new();
    let store = GroupStore::new(f.home.clone()).expect("store");
    f.group.group_id = LEGACY_DRAIN_GROUP.into();
    let original = json!({"id":"mail-drain","enabled":true,"action":{"kind":"notify","message":"keep this definition"},"trigger":{"kind":"interval","every_seconds":300},"to":["peer"]});
    let unrelated = json!({"id":"other","enabled":true,"action":{"kind":"notify"},"trigger":{"kind":"interval","every_seconds":300}});
    f.group
        .automation
        .insert("rules".into(), json!([original, unrelated]));
    store.save(&f.group).expect("save");
    std::fs::File::create(f.path()).expect("ledger");
    retire_legacy_rule(&f.home, LEGACY_DRAIN_GROUP).expect("retire");
    retire_legacy_rule(&f.home, LEGACY_DRAIN_GROUP).expect("repeat");
    let g = store.load(LEGACY_DRAIN_GROUP).expect("group");
    assert_eq!(g.automation["rules"][0]["enabled"], false);
    assert_eq!(g.automation["rules"][1], unrelated);
    let es = ledger::read_all(&f.path()).expect("events");
    assert_eq!(es.len(), 1);
    assert_eq!(es[0].data["previous_rule"], original);
    let mut queued = Event::new("system.notify", LEGACY_DRAIN_GROUP);
    queued.data = json!({"kind":"automation","context":{"rule_id":"mail-drain"}})
        .as_object()
        .cloned()
        .expect("data");
    assert!(is_mail_originated(LEGACY_DRAIN_GROUP, &queued));
    assert!(!is_mail_originated("g_other", &queued));
    queued.data["context"]["rule_id"] = json!("other");
    assert!(!is_mail_originated(LEGACY_DRAIN_GROUP, &queued));
}

#[test]
fn failed_submission_backs_off_without_spending_wakeup_and_carrier_retry_omits_hint() {
    let f = Fixture::new();
    f.mail(f.now, &["peer"]);
    reserve_boundary_turn(
        &f.home,
        &f.group.group_id,
        "peer",
        "failed-source",
        f.now + 300,
    )
    .expect("reserve")
    .expect("hint");
    finish_carrier(
        &f.home,
        &f.group.group_id,
        "peer",
        "failed-source",
        "failed",
        f.now + 301,
    )
    .expect("finish");
    let state = f.summary(f.now + 301).state;
    assert_eq!(state.standalone_wakeups, 0);
    assert_eq!(state.attempt_no, 1);
    assert!(
        reserve_boundary_turn(
            &f.home,
            &f.group.group_id,
            "peer",
            "failed-source",
            state.due_at
        )
        .expect("retry")
        .is_none()
    );
    assert!(
        reserve_boundary_turn(
            &f.home,
            &f.group.group_id,
            "peer",
            "new-source",
            state.due_at
        )
        .expect("new")
        .is_some()
    );
}

#[test]
fn revalidation_removes_hint_after_read_or_expiry_before_input() {
    for expire in [false, true] {
        let f = Fixture::new();
        let mail = f.mail(f.now, &["peer"]);
        let hint = reserve_delivery_hint(
            &f.home,
            &f.group.group_id,
            "peer",
            &["ordinary".into()],
            f.now + 300,
        )
        .expect("reserve")
        .expect("hint");
        assert_eq!(
            validate_delivery_hint(&f.home, &f.group.group_id, "peer", &hint.token, f.now + 301)
                .expect("valid"),
            Some(1)
        );
        if !expire {
            f.read(&mail, f.now + 302);
        }
        let at = if expire {
            f.now + EXPIRY_SECONDS
        } else {
            f.now + 302
        };
        assert!(
            validate_delivery_hint(&f.home, &f.group.group_id, "peer", &hint.token, at)
                .expect("revalidate")
                .is_none()
        );
        assert_eq!(f.summary(at).state.standalone_wakeups, 0);
    }
}

#[test]
fn legacy_receipts_seed_budget_and_unverified_notices_suppress_standalone() {
    for accepted in [false, true] {
        let f = Fixture::new();
        let mail = f.mail(f.now, &["peer"]);
        let mut old = Event::new("system.notify", &f.group.group_id);
        old.ts = f.ts(f.now + 100);
        old.data=json!({"kind":"mail_notice","target_actor_id":"peer","context":{"source_event_ids":[mail]}}).as_object().cloned().expect("data");
        let id = f.event(old);
        if accepted {
            let mut receipt = Event::new("runtime.delivery", &f.group.group_id);
            receipt.ts = f.ts(f.now + 100);
            receipt.data = json!({"actor_id":"peer","source_event_id":id,"state":"accepted"})
                .as_object()
                .cloned()
                .expect("data");
            f.event(receipt);
        }
        scan(&f.home, &f.group.group_id, f.now + 300).expect("scan");
        let s = f.summary(f.now + 300).state;
        assert_eq!(s.standalone_wakeups, u32::from(accepted));
        assert_eq!(s.legacy_unverified, !accepted);
        if !accepted {
            assert!(
                reserve_boundary_turn(&f.home, &f.group.group_id, "peer", "new", f.now + 10000)
                    .expect("reserve")
                    .is_none()
            );
            assert!(f.offer("passive", f.now + 10000).is_some());
        }
    }
}

#[test]
fn backoff_is_stable_bounded_and_has_no_tight_clock_rollback_loop() {
    let f = Fixture::new();
    f.mail(f.now, &["peer"]);
    let mut at = f.now + 300;
    for i in 1..=12 {
        assert!(f.offer(&format!("context-{i}"), at).is_some());
        let due = f.summary(at).state.due_at;
        let interval = backoff("peer", i);
        assert_eq!(due - at, interval);
        assert!(interval <= 6 * 3600 + 30);
        assert!(f.offer("rollback", at - 1).is_none());
        at = due;
    }
}

#[test]
fn corrupt_relevant_stored_bytes_cannot_disappear_into_permissive_history() {
    use std::io::Write;
    let f = Fixture::new();
    f.mail(f.now, &["peer"]);
    f.offer("first", f.now + 300).expect("hint");
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(f.path())
        .expect("file");
    file.write_all(b"{\"kind\":\"mail.attention\",\"data\":\n")
        .expect("corrupt");
    file.sync_all().expect("sync");
    assert!(
        offer_context(
            &f.home,
            &f.group.group_id,
            "peer",
            "after-corruption",
            f.now + 10000
        )
        .is_err()
    );
    assert!(inspect(&f.home, &f.group.group_id, "peer", f.now + 10000).is_err());
}

#[test]
fn cache_write_failure_cannot_strand_or_reset_a_committed_presentation() {
    let f = Fixture::new();
    f.mail(f.now, &["peer"]);
    let cache = GroupStore::new(f.home.clone())
        .expect("store")
        .group_dir(&f.group.group_id)
        .expect("dir")
        .join("state/mail-attention.json");
    std::fs::create_dir_all(&cache).expect("obstruct optional cache");
    let hint = reserve_boundary_turn(&f.home, &f.group.group_id, "peer", "native", f.now + 300)
        .expect("durable reservation")
        .expect("hint");
    assert!(!hint.token.is_empty());
    finish_carrier(
        &f.home,
        &f.group.group_id,
        "peer",
        "native",
        "accepted",
        f.now + 301,
    )
    .expect("durable result");
    assert_eq!(f.summary(f.now + 301).state.standalone_wakeups, 1);
    std::fs::remove_dir(cache).expect("repair cache");
    scan(&f.home, &f.group.group_id, f.now + 302).expect("rebuild");
    assert_eq!(f.summary(f.now + 302).state.standalone_wakeups, 1);
}
