use super::*;
use serde_json::json;

const KEY: &str = "OPENCODE_CONFIG_CONTENT";

fn merged(actor: Option<&str>, inherited: Option<&str>, snapshot: bool) -> Value {
    let mut env = BTreeMap::new();
    if let Some(content) = actor {
        env.insert(KEY.into(), content.into());
    }
    merge_snapshot_config(&mut env, KEY, inherited, snapshot).expect("config merge");
    serde_json::from_str(&env[KEY]).expect("merged object")
}

#[test]
fn opencode_and_kilo_use_their_respective_inline_config_variables() {
    assert_eq!(snapshot_config_key(ActorRuntime::Opencode), Some(KEY));
    assert_eq!(
        snapshot_config_key(ActorRuntime::Kilo),
        Some("KILO_CONFIG_CONTENT")
    );
    assert_eq!(snapshot_config_key(ActorRuntime::Codex), None);
    assert_eq!(snapshot_config_key(ActorRuntime::Custom), None);
}

#[test]
fn snapshots_default_off_and_preserve_nested_provider_and_mcp_config() {
    let original = json!({"provider":{"example":{"options":{"apiKey":"private"}}},
        "mcp":{"fixture":{"command":["example"]}},"model":"example/test"});
    let actual = merged(Some(&original.to_string()), None, false);
    assert_eq!(actual["snapshot"], false);
    for key in ["provider", "mcp", "model"] {
        assert_eq!(actual[key], original[key]);
    }
    assert_eq!(merged(None, None, false), json!({"snapshot":false}));
    assert_eq!(merged(Some(""), None, false), json!({"snapshot":false}));
}

#[test]
fn explicit_actor_snapshot_overrides_the_default() {
    assert_eq!(
        merged(Some(r#"{"snapshot":true}"#), None, false)["snapshot"],
        true
    );
    assert_eq!(
        merged(Some(r#"{"snapshot":false}"#), None, true)["snapshot"],
        false
    );
}

#[test]
fn inherited_config_is_preserved_but_its_snapshot_does_not_override_launch_policy() {
    assert_eq!(
        merged(
            None,
            Some(r#"{"snapshot":true,"model":"example/test"}"#),
            false
        ),
        json!({"snapshot":false,"model":"example/test"})
    );
    assert_eq!(
        merged(
            Some(r#"{"model":"actor/test"}"#),
            Some(r#"{"model":"daemon/test","snapshot":true}"#),
            false
        ),
        json!({"snapshot":false,"model":"actor/test"})
    );
}

#[test]
fn invalid_inline_config_is_not_replaced_or_exposed_in_errors() {
    for content in ["{private-credential", "[]", "null", "false"] {
        let mut env = BTreeMap::from([(KEY.into(), content.into())]);
        let original = env.clone();
        let error = merge_snapshot_config(&mut env, KEY, None, false).expect_err("invalid config");
        assert_eq!(env, original);
        assert_eq!(error.code, "invalid_runtime_config");
        assert!(!error.message.contains(content));
    }
}

#[test]
fn group_snapshot_policy_reaches_both_runtime_launch_environments() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    let store = cccc_core::GroupStore::new(home.clone()).expect("store");
    let mut group = store.create("snapshot launch", "").expect("group");
    for runtime in [ActorRuntime::Opencode, ActorRuntime::Kilo] {
        let key = snapshot_config_key(runtime).expect("runtime key");
        let mut actor = Actor::new("snapshot-peer");
        actor.runtime = runtime;
        // Explicit empty actor config keeps the test independent of daemon env.
        actor.env.insert(key.into(), "{}".into());
        for enabled in [false, true] {
            group
                .extra
                .insert("settings".into(), json!({"opencode_snapshot":enabled}));
            let env = launch_env(&home, &group, &actor).expect("launch env");
            let config: Value = serde_json::from_str(&env[key]).expect("config");
            assert_eq!(config["snapshot"], enabled);
            assert_eq!(env["CCCC_GROUP_ID"], group.group_id);
            assert_eq!(env["CCCC_ACTOR_ID"], actor.id);
        }
        actor.env.insert(key.into(), r#"{"snapshot":false}"#.into());
        let env = launch_env(&home, &group, &actor).expect("actor override");
        assert_eq!(
            serde_json::from_str::<Value>(&env[key]).expect("config")["snapshot"],
            false
        );
    }
}
