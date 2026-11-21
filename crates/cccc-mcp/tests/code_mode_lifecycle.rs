//! Code mode in an isolated Home: a Web Model caller, the only kind code mode
//! serves, is refused before any cell or nested command starts.
#![cfg(unix)]
use cccc_contracts::{Actor, ActorRuntime, DaemonRequest};
use cccc_core::{GroupStore, HomeLayout};
use serde_json::{Value, json};
use std::time::Duration;

async fn call(home: &HomeLayout, group: &str, name: &str, args: Value) -> Value {
    cccc_mcp::handle_request_for_actor(home,
        &json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}}),
        group, "web").await["result"].clone()
}

#[tokio::test]
async fn code_mode_and_nested_commands_are_refused_to_web_model() {
    let temp = tempfile::tempdir().expect("fixture");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    home.initialize().expect("initialize");
    let root = temp.path().join("project");
    std::fs::create_dir(&root).expect("workspace");
    let store = GroupStore::new(home.clone()).expect("store");
    let mut group = store.create("code lifecycle", "").expect("group");
    let mut actor = Actor::new("web");
    actor.runtime = ActorRuntime::WebModel;
    actor.normalize_runtime_constraints();
    cccc_core::actors::add(&mut group, actor).expect("actor");
    store.save(&group).expect("save");
    cccc_core::group_scope::attach(
        &store,
        &group.group_id,
        cccc_core::Scope {
            scope_key: "project".into(),
            url: root.to_string_lossy().into_owned(),
            label: "project".into(),
            git_remote: String::new(),
        },
    )
    .expect("scope");
    let daemon_home = home.clone();
    let daemon = tokio::spawn(async move { cccc_daemon::run(daemon_home).await });
    let client = cccc_client::DaemonClient::new(home.clone());
    tokio::time::timeout(Duration::from_secs(5), async {
        while client
            .call(&DaemonRequest {
                v: 1,
                op: "ping".into(),
                args: Default::default(),
            })
            .await
            .is_err()
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("ready");
    let source = "text(await tools.cccc_shell({command:\"sh -c 'printf x >> calls'\"}));";
    for (name, args) in [
        (
            "cccc_code_exec",
            json!({"source":source,"yield_time_ms":100}),
        ),
        ("cccc_code_wait", json!({"cell_id":"1","yield_time_ms":100})),
    ] {
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            call(&home, &group.group_id, name, args),
        )
        .await
        .expect("refusal needs no cell");
        assert_eq!(result["isError"], true, "{name}: {result}");
        assert!(
            result
                .to_string()
                .contains("not available to Web Model actors"),
            "{name}: {result}"
        );
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!root.join("calls").exists(), "no nested command may run");
    cccc_mcp::shutdown(&home).await;
        } else {
            let stopped = call(
                &home,
                &group.group_id,
                "cccc_code_wait",
                json!({"cell_id":id,"terminate":true}),
            )
            .await;
            assert_eq!(stopped["status"], "terminated");
        }
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(
            !root.join(format!("{prefix}.leaked")).exists(),
            "owned nested process must stop"
        );
    }
    cccc_mcp::shutdown(&home).await;
    daemon.abort();
    let _ = daemon.await;
}

cccc_daemon::session_owner_test_entry!();
