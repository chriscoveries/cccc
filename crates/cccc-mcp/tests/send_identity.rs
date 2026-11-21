//! T806 (seat ruling): an explicit sender that is not the connected actor is
//! refused with `permission_denied`, never normalised to the caller, and
//! nothing is appended. These drive the real MCP dispatch path
//! (`handle_request_for_actor`) against a live daemon, not the router helpers
//! alone — the defect was that the router rewrote `by` before the daemon ever
//! saw the spoof.

use cccc_client::DaemonClient;
use cccc_contracts::DaemonRequest;
use cccc_core::{GroupStore, HomeLayout};
use serde_json::{Map, Value, json};

#[tokio::test]
async fn a_mismatched_sender_is_refused_and_appends_nothing() {
    let (_temp, home, group_id) = fixture("t806-refuse");
    let daemon = start_daemon(&home).await;

    let refused = send_as(&home, &group_id, 1, Some("peer2")).await;
    assert_eq!(
        refused["result"]["isError"], true,
        "a forged sender must be an error, got {refused}"
    );
    assert_eq!(
        refused["result"]["structuredContent"]["error"]["code"], "permission_denied",
        "refused with the code callers already handle, got {refused}"
    );
    assert!(
        refused["result"].get("structuredContent").is_some(),
        "the refusal keeps the daemon's error shape: {refused}"
    );

    // Nothing landed under either name: not the spoof, not the caller.
    let history = history_as(&home, &group_id, 2).await;
    assert_eq!(
        history["result"]["structuredContent"]["messages"],
        json!([]),
        "a refused send must leave the ledger empty"
    );

    // The same connection still works when it tells the truth.
    let accepted = send_as(&home, &group_id, 3, Some("peer1")).await;
    assert!(
        accepted["result"].get("isError").is_none(),
        "the caller's own identity must still send: {accepted}"
    );

    // And an omitted sender is still filled in from the connection.
    let omitted = send_as(&home, &group_id, 4, None).await;
    assert!(
        omitted["result"].get("isError").is_none(),
        "an omitted sender is normalised, not refused: {omitted}"
    );
    let event = &omitted["result"]["structuredContent"]["event"];
    assert_eq!(event["by"], "peer1");
    assert_eq!(event["data"]["actor_id"], "peer1");

    daemon.abort();
}

async fn send_as(home: &HomeLayout, group_id: &str, id: u64, by: Option<&str>) -> Value {
    let mut arguments = json!({
        "group_id": group_id,
        "to": ["peer2"],
        "text": format!("t806 send {id}"),
        "insight": "Refusal and acceptance are measured on the same path, so the control proves the daemon still accepts an honest sender.",
        "message_mode": "send",
    });
    if let Some(by) = by {
        arguments["by"] = json!(by);
    }
    mcp_call(home, group_id, id, "cccc_message_send", arguments).await
}

async fn history_as(home: &HomeLayout, group_id: &str, id: u64) -> Value {
    mcp_call(
        home,
        group_id,
        id,
        "cccc_message_history",
        json!({"mode": "send"}),
    )
    .await
}

async fn mcp_call(
    home: &HomeLayout,
    group_id: &str,
    id: u64,
    name: &str,
    arguments: Value,
) -> Value {
    cccc_mcp::handle_request_for_actor(
        home,
        &json!({
            "jsonrpc":"2.0","id":id,"method":"tools/call",
            "params":{"name":name,"arguments":arguments}
        }),
        group_id,
        "peer1",
    )
    .await
}

fn fixture(name: &str) -> (tempfile::TempDir, HomeLayout, String) {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = HomeLayout::from_path(temp.path().join(name)).expect("home");
    home.initialize().expect("initialize home");
    let store = GroupStore::new(home.clone()).expect("store");
    let mut group = store.create(name, "").expect("group");
    for actor in ["peer1", "peer2"] {
        cccc_core::actors::add(&mut group, cccc_contracts::Actor::new(actor)).expect("actor");
    }
    store.save(&group).expect("save group");
    let group_id = group.group_id.clone();
    (temp, home, group_id)
}

async fn start_daemon(home: &HomeLayout) -> tokio::task::JoinHandle<Result<(), anyhow::Error>> {
    let daemon_home = home.clone();
    let task = tokio::spawn(async move { cccc_daemon::run(daemon_home).await });
    let client = DaemonClient::new(home.clone());
    for _ in 0..100 {
        if client
            .call(&DaemonRequest {
                v: 1,
                op: "group_list".into(),
                args: Map::new(),
            })
            .await
            .is_ok()
        {
            return task;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("daemon did not start");
}
