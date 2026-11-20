use super::*;
use cccc_contracts::{Actor, ActorRuntime};
use serde_json::Value;

fn request(op: &str, args: Value) -> DaemonRequest {
    DaemonRequest {
        v: 1,
        op: op.into(),
        args: args.as_object().expect("valid test fixture").clone(),
    }
}
#[test]
fn pairing_requires_user_stopped_actor_and_confirmation_and_preserves_pending_delivery() {
    for status in [
        "submitting",
        "deferred",
        "pending_new_chat_bind",
        "legacy_recovery_submitting",
        "submission_ambiguous",
        "submission_ambiguous_completion_pending",
        "completion_ambiguous",
        "legacy_submission_unverified",
        "ambiguous",
        "completion_conflict",
    ] {
        let temp = tempfile::tempdir().expect("valid test fixture");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("valid test fixture");
        home.initialize().expect("valid test fixture");
        let groups = GroupStore::new(home.clone()).expect("valid test fixture");
        let mut group = groups.create("pairing", "").expect("valid test fixture");
        let mut actor = Actor::new("a");
        actor.runtime = ActorRuntime::WebModel;
        actor.enabled = true;
        cccc_core::actors::add(&mut group, actor).expect("valid test fixture");
        groups.save(&group).expect("valid test fixture");
        assert!(
            execute(
                &home,
                &request("web_model_connector_configure", json!({"by":"a"}))
            )
            .is_err()
        );
        let configured = execute(
            &home,
            &request("web_model_connector_configure", json!({"by":"user"})),
        )
        .expect("valid test fixture");
        let id = configured["connector"]["connector_id"]
            .as_str()
            .expect("valid test fixture");
        let args = json!({"by":"user","connector_id":id,"group_id":group.group_id,"actor_id":"a"});
        assert!(execute(&home, &request("web_model_pairing_begin", args.clone())).is_err());
        groups
            .mutate(&group.group_id, |g| {
                g.actors[0].enabled = false;
                Ok(())
            })
            .expect("valid test fixture");
        integration_state::group_update(&groups,&group.group_id,"web_model_browser_targets", |v| { *v=json!({"a":{"url":"https://chatgpt.com/c/saved","last_delivery_status":status,"last_delivery_id":"receipt-1"}}); Ok(()) }).expect("valid test fixture");
        let pair = execute(&home, &request("web_model_pairing_begin", args.clone()))
            .expect("valid test fixture");
        execute(
            &home,
            &request(
                "web_model_pairing_accept",
                json!({"by":"user","connector_id":id,"code":pair["code"],"session_key":"host-key"}),
            ),
        )
        .expect("valid test fixture");
        assert!(
            store::binding_for_session(
                &store::load(&home).expect("valid test fixture")[0],
                "host-key"
            )
            .is_none()
        );
        let mut confirm = args.clone();
        confirm["pairing_id"] = pair["pairing_id"].clone();
        confirm["url"] = json!("https://chatgpt.com/c/other");
        assert!(
            execute(
                &home,
                &request("web_model_pairing_confirm", confirm.clone())
            )
            .is_err()
        );
        confirm["url"] = json!("https://chatgpt.com/c/saved");
        execute(&home, &request("web_model_pairing_confirm", confirm)).expect("valid test fixture");
        assert!(
            store::binding_for_session(
                &store::load(&home).expect("valid test fixture")[0],
                "host-key"
            )
            .is_some()
        );
        assert_eq!(
            integration_state::group_get(&groups, &group.group_id, "web_model_browser_targets")
                .expect("valid test fixture")["a"]["last_delivery_id"],
            "receipt-1"
        );
        assert!(execute(&home, &request("web_model_binding_remove", args)).is_err());
    }
}

#[test]
fn automatic_pairing_requires_running_group_and_never_replaces_existing_binding() {
    let temp = tempfile::tempdir().expect("temp");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    home.initialize().expect("init");
    let groups = GroupStore::new(home.clone()).expect("groups");
    let mut group = groups.create("automatic", " ").expect("group");
    let mut actor = Actor::new("a");
    actor.runtime = ActorRuntime::WebModel;
    actor.enabled = true;
    cccc_core::actors::add(&mut group, actor).expect("actor");
    groups.save(&group).expect("save");
    let configured = store::configure(&home).expect("configure");
    let id = &configured["connector"]["connector_id"];
    let args = json!({"by":"user","connector_id":id,"group_id":group.group_id,"actor_id":"a","automatic":true});
    assert!(execute(&home, &request("web_model_pairing_begin", args.clone())).is_err());
    groups
        .mutate(&group.group_id, |g| {
            g.running = true;
            g.state = GroupState::Active;
            Ok(())
        })
        .expect("start group");
    let p =
        execute(&home, &request("web_model_pairing_begin", args.clone())).expect("automatic begin");
    execute(
        &home,
        &request(
            "web_model_pairing_accept",
            json!({"by":"user","connector_id":id,"code":p["code"],"session_key":"host"}),
        ),
    )
    .expect("accept");
    let mut confirm = args.clone();
    confirm["pairing_id"] = p["pairing_id"].clone();
    confirm["url"] = json!("https://chatgpt.com/c/a");
    let first = execute(
        &home,
        &request("web_model_pairing_confirm", confirm.clone()),
    )
    .expect("confirm running actor");
    assert_eq!(
        first,
        execute(&home, &request("web_model_pairing_confirm", confirm)).expect("idempotent confirm")
    );
    assert!(
        execute(&home, &request("web_model_pairing_begin", args.clone())).is_err(),
        "auto cannot replace bound conversation"
    );
    store::retire_actor(&home, &group.group_id, "a").expect("retire");
    groups
        .mutate(&group.group_id, |g| {
            g.state = GroupState::Paused;
            Ok(())
        })
        .expect("pause");
    assert!(execute(&home, &request("web_model_pairing_begin", args)).is_err());
}

/// cwf#821: re-pairing a running Actor must never unpair it first. With
/// `replace: true` the old binding stays in force through a failed or
/// abandoned attempt and is only superseded by the new confirm.
#[test]
fn explicit_replace_repairs_without_removing_the_working_binding() {
    let temp = tempfile::tempdir().expect("temp");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    home.initialize().expect("init");
    let groups = GroupStore::new(home.clone()).expect("groups");
    let mut group = groups.create("replace", " ").expect("group");
    let mut actor = Actor::new("a");
    actor.runtime = ActorRuntime::WebModel;
    actor.enabled = true;
    cccc_core::actors::add(&mut group, actor).expect("actor");
    group.running = true;
    group.state = GroupState::Active;
    groups.save(&group).expect("save");
    let configured = store::configure(&home).expect("configure");
    let id = configured["connector"]["connector_id"].clone();
    let args = json!({"by":"user","connector_id":id,"group_id":group.group_id,"actor_id":"a","automatic":true});
    let pair_and_confirm = |args: &Value, session: &str, url: &str| {
        let p = execute(&home, &request("web_model_pairing_begin", args.clone())).expect("begin");
        execute(
            &home,
            &request(
                "web_model_pairing_accept",
                json!({"by":"user","connector_id":id,"code":p["code"],"session_key":session}),
            ),
        )
        .expect("accept");
        let mut confirm = args.clone();
        confirm["pairing_id"] = p["pairing_id"].clone();
        confirm["url"] = json!(url);
        (p, confirm)
    };
    let (_, confirm) = pair_and_confirm(&args, "host-1", "https://chatgpt.com/c/first");
    execute(&home, &request("web_model_pairing_confirm", confirm)).expect("first bind");
    let bound_url = |home: &HomeLayout| {
        store::binding_for_actor(
            &store::load(home).expect("store")[0],
            &group.group_id,
            "a",
            &cccc_core::actors::generation_identity(
                &groups.load(&group.group_id).expect("group").actors[0],
            ),
        )
        .map(|b| b["url"].clone())
    };
    assert_eq!(bound_url(&home), Some(json!("https://chatgpt.com/c/first")));

    // Default behaviour is unchanged: automatic begin cannot replace.
    let refused = execute(&home, &request("web_model_pairing_begin", args.clone()))
        .expect_err("automatic begin without replace");
    assert_eq!(refused.code, "already_paired");

    // An explicit replace begins without touching the binding; a failed
    // attempt (e.g. the browser hit a 429) leaves it exactly as it was.
    let mut replace = args.clone();
    replace["replace"] = json!(true);
    let attempt = execute(&home, &request("web_model_pairing_begin", replace.clone()))
        .expect("replace begin");
    assert_eq!(bound_url(&home), Some(json!("https://chatgpt.com/c/first")));
    let mut failed = replace.clone();
    failed["pairing_id"] = attempt["pairing_id"].clone();
    failed["error_code"] = json!("pairing_interrupted");
    execute(&home, &request("web_model_pairing_fail", failed)).expect("fail attempt");
    assert_eq!(
        bound_url(&home),
        Some(json!("https://chatgpt.com/c/first")),
        "a failed re-pair must not unpair the Actor"
    );

    // A confirm without replace still cannot supersede the binding.
    let (_, confirm) = pair_and_confirm(&replace, "host-2", "https://chatgpt.com/c/second");
    let mut plain = confirm.clone();
    plain.as_object_mut().expect("object").remove("replace");
    assert_eq!(
        execute(&home, &request("web_model_pairing_confirm", plain))
            .expect_err("plain confirm")
            .code,
        "already_paired"
    );
    assert_eq!(bound_url(&home), Some(json!("https://chatgpt.com/c/first")));
    // The replace confirm supersedes it in one write.
    execute(&home, &request("web_model_pairing_confirm", confirm)).expect("replace confirm");
    assert_eq!(
        bound_url(&home),
        Some(json!("https://chatgpt.com/c/second"))
    );
}

#[test]
fn legacy_actor_pairing_keeps_the_creation_identity() {
    let temp = tempfile::tempdir().expect("temp");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    home.initialize().expect("init");
    let groups = GroupStore::new(home.clone()).expect("groups");
    let mut group = groups.create("legacy pairing", "").expect("group");
    let mut actor = Actor::new("a");
    actor.runtime = ActorRuntime::WebModel;
    actor.enabled = true;
    let created_at = actor.created_at.clone();
    group.actors.push(actor);
    group.running = true;
    group.state = GroupState::Active;
    groups.save(&group).expect("save legacy actor");
    let connector = store::configure(&home).expect("connector");
    let id = &connector["connector"]["connector_id"];
    let pair = execute(
        &home,
        &request(
            "web_model_pairing_begin",
            json!({"by":"user","connector_id":id,"group_id":group.group_id,"actor_id":"a","automatic":true}),
        ),
    )
    .expect("start pairing");
    let persisted = groups.load(&group.group_id).expect("reload");
    assert!(
        persisted.actors[0].generation.is_empty(),
        "pairing must not replace a legacy Actor's identity after its browser opens"
    );
    let saved = store::load(&home).expect("store");
    let attempt = store::pairing_for_actor(&saved[0], &group.group_id, "a").expect("attempt");
    assert_eq!(attempt["pairing_id"], pair["pairing_id"]);
    assert_eq!(attempt["generation"], format!("legacy:{created_at}"));

    store::accept_pairing(
        &home,
        id.as_str().expect("connector ID"),
        pair["code"].as_str().expect("code"),
        "host-session",
    )
    .expect("accept");
    execute(
        &home,
        &request(
            "web_model_pairing_confirm",
            json!({"by":"user","connector_id":id,"group_id":group.group_id,"actor_id":"a",
                "pairing_id":pair["pairing_id"],"url":"https://chatgpt.com/c/legacy"}),
        ),
    )
    .expect("confirm");
    let saved = store::load(&home).expect("reload connector");
    let mut binding = store::binding_for_session(&saved[0], "host-session").expect("binding");
    binding["connector_id"] = id.clone();
    store::validate_binding(&home, &binding).expect("legacy Actor remains authorized");
}

#[test]
fn stopping_a_cli_actor_does_not_depend_on_the_web_connector_store() {
    let temp = tempfile::tempdir().expect("temp");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    home.initialize().expect("init");
    let groups = GroupStore::new(home.clone()).expect("groups");
    let mut group = groups.create("CLI", "").expect("group");
    let actor = Actor::new("cli");
    assert_ne!(actor.runtime, ActorRuntime::WebModel);
    group.actors.push(actor);
    std::fs::write(
        home.root().join("web_model_connectors.yaml"),
        "version: 999\n",
    )
    .expect("unreadable Web connector");
    assert!(super::super::actor_runtime::apply(&home, &group, "cli", "actor.stop").is_ok());
    assert!(super::super::actor_runtime::stop_group(&home, &group).is_ok());
}

#[test]
fn grok_binding_preserves_enabled_state_and_retires_on_runtime_change() {
    let temp = tempfile::tempdir().expect("valid test fixture");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("valid test fixture");
    home.initialize().expect("valid test fixture");
    let groups = GroupStore::new(home.clone()).expect("valid test fixture");
    let mut group = groups.create("Grok", "").expect("valid test fixture");
    let mut actor = Actor::new("bot");
    actor.runtime = ActorRuntime::GrokWebModel;
    actor.enabled = true;
    cccc_core::actors::add(&mut group, actor).expect("valid test fixture");
    groups.save(&group).expect("valid test fixture");
    let chatgpt = store::configure(&home).expect("valid test fixture");
    let configured = execute(
        &home,
        &request(
            "web_model_connector_configure",
            json!({"by":"user","provider":"grok_web"}),
        ),
    )
    .expect("valid test fixture");
    assert!(
        execute(
            &home,
            &request(
                "web_model_connector_configure",
                json!({"by":"user","provider":false})
            )
        )
        .is_err()
    );
    let id = &configured["connector"]["connector_id"];
    let mut args = json!({"by":"user","connector_id":id,"group_id":group.group_id,"actor_id":"bot","url":"https://grok.com/bot/1373170d-9cf2-408c-b597-e243e5884f4a"});
    execute(&home, &request("web_model_grok_bind", args.clone())).expect("idle native Actor");
    assert!(groups.load(&group.group_id).expect("group").actors[0].enabled);
    assert!(execute(&home, &request("web_model_binding_remove", args.clone())).is_err());
    groups
        .mutate(&group.group_id, |g| {
            g.actors[0].enabled = false;
            Ok(())
        })
        .expect("valid test fixture");
    execute(&home, &request("web_model_grok_bind", args.clone())).expect("valid test fixture");
    let connector = || {
        store::load(&home)
            .expect("valid test fixture")
            .into_iter()
            .find(|c| &c["connector_id"] == id)
            .expect("valid test fixture")
    };
    let binding = connector()["bindings"]
        .as_object()
        .expect("bindings")
        .values()
        .next()
        .expect("binding")
        .clone();
    // Browser integration leftovers must not obstruct native setup. Canonical
    // unfinished turns, rather than browser UI metadata, protect active work.
    integration_state::group_update(&groups, &group.group_id, "web_model_browser_targets", |v| {
        *v = json!({"bot":{"last_delivery_status":"submission_ambiguous","last_delivery_id":"legacy"}});
        Ok(())
    }).expect("legacy metadata");
    execute(&home, &request("web_model_grok_bind", args.clone())).expect("same native route");
    assert_eq!(
        connector()["bindings"]
            .as_object()
            .expect("bindings")
            .values()
            .next()
            .expect("binding")["revision"],
        binding["revision"]
    );
    execute(&home, &request("web_model_binding_remove", args.clone()))
        .expect("remove native route");
    execute(&home, &request("web_model_grok_bind", args.clone())).expect("rebind native route");
    for runtime in ["web_model", "grok_web_model"] {
        let r = request(
            "actor_update",
            json!({"by":"user","group_id":group.group_id,"actor_id":"bot","patch":{"runtime":runtime}}),
        );
        super::super::actors::resolve_operation(&r)
            .expect("valid test fixture")
            .execute(&home, &r)
            .expect("valid test fixture");
    }
    assert!(
        store::binding_for_actor(
            &connector(),
            &group.group_id,
            "bot",
            &cccc_core::actors::generation_identity(
                &groups.load(&group.group_id).expect("group").actors[0]
            )
        )
        .is_none(),
        "runtime replacement retires the binding"
    );
    args["connector_id"] = chatgpt["connector"]["connector_id"].clone();
    assert!(execute(&home, &request("web_model_grok_bind", args)).is_err());
    assert_eq!(
        store::load(&home)
            .expect("valid test fixture")
            .into_iter()
            .find(|c| c["provider"] == "chatgpt_web")
            .expect("valid test fixture")["secret_hash"],
        chatgpt["connector"]["secret_hash"]
    );
}

#[test]
fn non_rotating_flags_are_idempotent_and_malformed_flags_refuse() {
    let temp = tempfile::tempdir().expect("valid test fixture");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("valid test fixture");
    home.initialize().expect("valid test fixture");
    store::configure(&home).expect("existing connector");
    let before = store::load(&home).expect("load");
    for args in [
        serde_json::json!({"by":"user","provider":"chatgpt_web"}),
        serde_json::json!({"by":"user","provider":"chatgpt_web","create_only":true}),
        serde_json::json!({"by":"user","provider":"chatgpt_web","create_only":false}),
        serde_json::json!({"by":"user","provider":"chatgpt_web","rotate":false}),
        serde_json::json!({"by":"user","provider":"chatgpt_web","rotate":false,"create_only":true}),
    ] {
        let existing = execute(&home, &request("web_model_connector_configure", args))
            .expect("an existing connector is idempotent");
        assert_eq!(existing["outcome"], "existing");
        assert!(existing.get("secret").is_none());
        assert_eq!(store::load(&home).expect("load"), before, "nothing rotated");
    }
    for (name, value) in [
        ("create_only", serde_json::json!("true")),
        ("create_only", serde_json::json!(1)),
        ("create_only", Value::Null),
        ("rotate", serde_json::json!("true")),
        ("rotate", serde_json::json!(1)),
        ("rotate", Value::Null),
    ] {
        let mut args = serde_json::json!({"by":"user","provider":"chatgpt_web"});
        args[name] = value;
        let error = execute(&home, &request("web_model_connector_configure", args))
            .expect_err("a non-boolean flag is refused");
        assert_eq!(error.code, "invalid_argument", "{name}");
    }
    let error = execute(
        &home,
        &request(
            "web_model_connector_configure",
            serde_json::json!({"by":"user","provider":"chatgpt_web","rotate":true,"create_only":true}),
        ),
    )
    .expect_err("contradictory flags are refused before mutation");
    assert_eq!(error.code, "invalid_argument");
    assert_eq!(store::load(&home).expect("load"), before, "nothing rotated");
}

#[test]
fn explicit_rotation_ignores_supplied_attribution_and_records_the_daemon_principal() {
    let temp = tempfile::tempdir().expect("valid test fixture");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("valid test fixture");
    home.initialize().expect("valid test fixture");
    let created = store::configure(&home).expect("existing connector");
    let old_secret = created["secret"].as_str().expect("secret");
    let connector = store::load(&home).expect("load").remove(0);
    let confirmation = store::rotation_preview(&connector)["confirmation"]
        .as_str()
        .expect("confirmation")
        .to_owned();
    let rotated = execute(
        &home,
        &request(
            "web_model_connector_configure",
            serde_json::json!({"by":"user","provider":"chatgpt_web","rotate":true,
                "confirmation":confirmation,"trusted_rotated_by":"spoofed","rotated_by":"spoofed"}),
        ),
    )
    .expect("explicit rotation");
    assert_eq!(rotated["outcome"], "rotated");
    assert_eq!(rotated["connector"]["credential_generation"], 2);
    assert_eq!(rotated["connector"]["rotated_by"], "local:user");
    assert!(rotated["connector"]["secret_rotated_at"].is_string());
    let new_secret = rotated["secret"].as_str().expect("secret");
    assert!(!store::secret_matches(&rotated["connector"], old_secret));
    assert!(store::secret_matches(&rotated["connector"], new_secret));
    let after = store::load(&home).expect("load");
    let stale = execute(
        &home,
        &request(
            "web_model_connector_configure",
            serde_json::json!({"by":"user","provider":"chatgpt_web","rotate":true,
                "confirmation":confirmation}),
        ),
    )
    .expect_err("a replayed confirmation is stale");
    assert_eq!(stale.code, "rotation_confirmation_stale");
    assert_eq!(store::load(&home).expect("load"), after, "nothing rotated");
}

struct PreserveFixture {
    _temp: tempfile::TempDir,
    home: HomeLayout,
    groups: GroupStore,
    group_id: String,
    connector_id: String,
    binding_revision: String,
    pairing_id: String,
}

fn preserve_fixture(status: &str, delivery_id: &str) -> PreserveFixture {
    let temp = tempfile::tempdir().expect("valid test fixture");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("valid test fixture");
    home.initialize().expect("valid test fixture");
    let groups = GroupStore::new(home.clone()).expect("valid test fixture");
    let mut group = groups.create("preserve", "").expect("valid test fixture");
    let mut actor = Actor::new("a");
    actor.runtime = ActorRuntime::WebModel;
    actor.enabled = true;
    cccc_core::actors::add(&mut group, actor).expect("valid test fixture");
    groups.save(&group).expect("valid test fixture");
    let configured = execute(
        &home,
        &request("web_model_connector_configure", json!({"by":"user"})),
    )
    .expect("valid test fixture");
    let connector_id = configured["connector"]["connector_id"]
        .as_str()
        .expect("valid test fixture")
        .to_owned();
    groups
        .mutate(&group.group_id, |g| {
            g.actors[0].enabled = false;
            Ok(())
        })
        .expect("valid test fixture");
    integration_state::group_update(&groups, &group.group_id, "web_model_browser_targets", |v| {
        *v = json!({"a":{
            "url":"https://chatgpt.com/c/saved","kind":"existing_chat",
            "last_delivery_status":status,"last_delivery_id":delivery_id,
            "last_delivery_turn_id":"turn-1","last_delivery_event_ids":["e1","e2"],
            "last_delivery_at":"2026-11-20T00:00:00Z",
            "last_submission_evidence":{"submission_evidence":"message_echo"}
        }});
        Ok(())
    })
    .expect("valid test fixture");
    let args =
        json!({"by":"user","connector_id":connector_id,"group_id":group.group_id,"actor_id":"a"});
    let pair = execute(&home, &request("web_model_pairing_begin", args.clone()))
        .expect("valid test fixture");
    execute(
        &home,
        &request(
            "web_model_pairing_accept",
            json!({"by":"user","connector_id":connector_id,"code":pair["code"],"session_key":"host-key"}),
        ),
    )
    .expect("valid test fixture");
    let mut confirm = args.clone();
    confirm["pairing_id"] = pair["pairing_id"].clone();
    confirm["url"] = json!("https://chatgpt.com/c/saved");
    execute(&home, &request("web_model_pairing_confirm", confirm)).expect("valid test fixture");
    let connector = store::load(&home).expect("valid test fixture").remove(0);
    let group = groups.load(&group.group_id).expect("valid test fixture");
    let actor = group
        .actors
        .iter()
        .find(|a| a.id == "a")
        .expect("valid test fixture");
    let generation = cccc_core::actors::generation_identity(actor);
    let binding = store::binding_for_actor(&connector, &group.group_id, "a", &generation)
        .expect("valid test fixture");
    PreserveFixture {
        _temp: temp,
        home,
        groups,
        group_id: group.group_id.clone(),
        connector_id,
        binding_revision: binding["revision"].as_str().unwrap().to_owned(),
        pairing_id: binding["pairing_id"].as_str().unwrap().to_owned(),
    }
}

fn preserve_args(f: &PreserveFixture, delivery_id: &str, key: &str) -> Value {
    json!({
        "by":"user","connector_id":f.connector_id,"group_id":f.group_id,"actor_id":"a",
        "binding_revision":f.binding_revision,"pairing_id":f.pairing_id,
        "expected_delivery_id":delivery_id,
        "reason":"bound_conversation_unavailable","rationale":"bound chat deleted upstream",
        "acknowledge_unresolved":true,"idempotency_key":key
    })
}

fn binding_present(f: &PreserveFixture) -> bool {
    let connector = store::load(&f.home).expect("load").remove(0);
    let group = f.groups.load(&f.group_id).expect("group");
    let actor = group.actors.iter().find(|a| a.id == "a").expect("actor");
    let generation = cccc_core::actors::generation_identity(actor);
    store::binding_for_actor(&connector, &f.group_id, "a", &generation).is_some()
}

#[test]
fn preserve_remove_terminalizes_the_reviewed_delivery_and_retires_the_binding() {
    let f = preserve_fixture("submission_ambiguous", "d-1");
    let result = execute(
        &f.home,
        &request(
            "web_model_binding_remove_preserve_unresolved",
            preserve_args(&f, "d-1", "k-1"),
        ),
    )
    .expect("preserve remove");
    assert_eq!(result["removed"], true);
    assert_eq!(result["status"], "abandoned_unresolved");
    assert_eq!(result["delivery_id"], "d-1");
    assert_eq!(result["prior_status"], "submission_ambiguous");
    assert_eq!(
        result["binding_revision"].as_str().unwrap(),
        f.binding_revision
    );
    assert_eq!(result["replay_performed"], false);
    assert!(
        result["resolution_id"]
            .as_str()
            .unwrap()
            .starts_with("wmres_")
    );
    assert!(!binding_present(&f), "binding retired");
    let targets = integration_state::group_get(&f.groups, &f.group_id, "web_model_browser_targets")
        .expect("targets");
    let t = &targets["a"];
    assert_eq!(t["last_delivery_status"], "abandoned_unresolved");
    assert_eq!(t["last_delivery_id"], "d-1");
    assert_eq!(t["last_delivery_turn_id"], "turn-1");
    assert_eq!(t["last_delivery_event_ids"], json!(["e1", "e2"]));
    assert_eq!(t["url"], "https://chatgpt.com/c/saved");
    assert_eq!(t["last_delivery_resolution_id"], result["resolution_id"]);
    let resolutions =
        integration_state::group_get(&f.groups, &f.group_id, RESOLUTIONS_KEY).expect("resolutions");
    let records: Vec<&Value> = resolutions.as_object().unwrap().values().collect();
    assert_eq!(records.len(), 1);
    let record = records[0];
    assert_eq!(record["state"], "committed");
    assert_eq!(record["outcome"], "unresolved_preserved_no_replay");
    assert_eq!(record["reason"], "bound_conversation_unavailable");
    assert_eq!(record["rationale"], "bound chat deleted upstream");
    assert_eq!(record["operator"], "user");
    assert_eq!(record["delivery_id"], "d-1");
    assert_eq!(record["previous_status"], "submission_ambiguous");
    assert_eq!(record["source_event_ids"], json!(["e1", "e2"]));
    assert_eq!(record["receipt"]["resolution_id"], result["resolution_id"]);
}

#[test]
fn preserve_remove_refuses_stale_ineligible_and_unacknowledged_requests() {
    let f = preserve_fixture("submission_ambiguous", "d-1");
    let mut args = preserve_args(&f, "d-OTHER", "k-1");
    let error = execute(
        &f.home,
        &request("web_model_binding_remove_preserve_unresolved", args.clone()),
    )
    .expect_err("stale delivery refused");
    assert_eq!(error.code, "delivery_changed");
    args = preserve_args(&f, "d-1", "k-1");
    args["binding_revision"] = json!("stale");
    let error = execute(
        &f.home,
        &request("web_model_binding_remove_preserve_unresolved", args),
    )
    .expect_err("stale binding refused");
    assert_eq!(error.code, "binding_revision_changed");
    assert!(binding_present(&f));

    let active = preserve_fixture("submitting", "d-2");
    let error = execute(
        &active.home,
        &request(
            "web_model_binding_remove_preserve_unresolved",
            preserve_args(&active, "d-2", "k-2"),
        ),
    )
    .expect_err("in-flight work is not abandonable");
    assert_eq!(error.code, "delivery_active");
    assert!(binding_present(&active));

    let gated = preserve_fixture("ambiguous", "d-3");
    for (key, value) in [
        ("acknowledge_unresolved", json!(false)),
        ("reason", json!("whatever")),
        ("rationale", json!("   ")),
    ] {
        let mut args = preserve_args(&gated, "d-3", "k-3");
        args[key] = value;
        let error = execute(
            &gated.home,
            &request("web_model_binding_remove_preserve_unresolved", args),
        )
        .expect_err("unacknowledged preserve refused");
        assert_eq!(error.code, "preserve_reason_required", "{key}");
    }
    assert!(binding_present(&gated));
    let targets =
        integration_state::group_get(&gated.groups, &gated.group_id, "web_model_browser_targets")
            .expect("targets");
    assert_eq!(targets["a"]["last_delivery_status"], "ambiguous");
}

#[test]
fn preserve_remove_retries_return_the_same_receipt_and_conflicts_on_changed_identity() {
    let f = preserve_fixture("completion_ambiguous", "d-1");
    let first = execute(
        &f.home,
        &request(
            "web_model_binding_remove_preserve_unresolved",
            preserve_args(&f, "d-1", "k-1"),
        ),
    )
    .expect("first attempt");
    let retry = execute(
        &f.home,
        &request(
            "web_model_binding_remove_preserve_unresolved",
            preserve_args(&f, "d-1", "k-1"),
        ),
    )
    .expect("idempotent retry");
    assert_eq!(first, retry);
    let error = execute(
        &f.home,
        &request(
            "web_model_binding_remove_preserve_unresolved",
            preserve_args(&f, "d-OTHER", "k-1"),
        ),
    )
    .expect_err("changed identity on a used key");
    assert_eq!(error.code, "resolution_conflict");
    let resolutions =
        integration_state::group_get(&f.groups, &f.group_id, RESOLUTIONS_KEY).expect("resolutions");
    assert_eq!(resolutions.as_object().unwrap().len(), 1);
}

#[test]
fn preserve_remove_refuses_an_active_turn() {
    let f = preserve_fixture("submission_ambiguous", "d-1");
    integration_state::group_update(&f.groups, &f.group_id, "runtime_states", |v| {
        *v = json!({"a":{"active_turn_id":"t-9","status":"working","active_event_ids":["e"]}});
        Ok(())
    })
    .expect("runtime state");
    let error = execute(
        &f.home,
        &request(
            "web_model_binding_remove_preserve_unresolved",
            preserve_args(&f, "d-1", "k-1"),
        ),
    )
    .expect_err("active turn refused");
    assert_eq!(error.code, "delivery_active");
    assert!(binding_present(&f));
    let targets = integration_state::group_get(&f.groups, &f.group_id, "web_model_browser_targets")
        .expect("targets");
    assert_eq!(targets["a"]["last_delivery_status"], "submission_ambiguous");
}
