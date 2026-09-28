//! A filtered `cccc_task list` must reach the daemon's own filters and paging.
//!
//! Reproduced class (task-list scale): the MCP tool forwarded only `group_id`
//! and `task_id`, so `list status=planned` returned the entire board. This
//! drives the real tool path — router -> mapping -> daemon op — on a seeded
//! temp home, so a mapping that drops a declared filter fails here.

use cccc_core::{GroupStore, HomeLayout};
use serde_json::{Value, json};

async fn tool(home: &HomeLayout, group: &str, actor: &str, name: &str, args: Value) -> Value {
    cccc_mcp::handle_request_for_actor(
        home,
        &json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}}),
        group,
        actor,
    )
    .await
}

fn payload(response: &Value) -> &Value {
    assert_ne!(response["result"]["isError"], true, "{response}");
    assert!(response.get("error").is_none(), "{response}");
    &response["result"]["structuredContent"]
}

fn bytes(value: &Value) -> usize {
    serde_json::to_string(value).expect("serialize").len()
}

fn error_code(response: &Value) -> String {
    response["result"]["structuredContent"]["error"]["code"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

#[tokio::test]
async fn task_list_forwards_filters_and_paging_to_the_daemon() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = HomeLayout::from_path(temp.path()).expect("home");
    let groups = GroupStore::new(home.clone()).expect("groups");
    let mut group = groups.create("task list filters", "").expect("group");
    for id in ["lead", "peer", "lane-1", "lane-2", "lane-3"] {
        cccc_core::actors::add(&mut group, cccc_contracts::Actor::new(id)).expect("actor");
    }
    groups.save(&group).expect("save group");
    let daemon_home = home.clone();
    let daemon_task = tokio::spawn(async move { cccc_daemon::run(daemon_home).await });
    let client = cccc_client::DaemonClient::new(home.clone());
    for _ in 0..100 {
        if client
            .call(&cccc_contracts::DaemonRequest {
                v: 1,
                op: "ping".into(),
                args: Default::default(),
            })
            .await
            .is_ok()
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let outcome = tokio::spawn(async move {
        let id = group.group_id.as_str();
        // A realistic card: a title plus the notes volume our cards carry.
        let notes =
            "STATE: probe card. ".to_string() + &"evidence line with some words. ".repeat(40);
        // Six planned (one unassigned, one per lane) and six done. A peer may
        // only create cards assigned to itself, so each lane plants its own;
        // the unassigned card is planted by the lead.
        const SEED: [(&str, &str); 12] = [
            ("planned", ""),
            ("planned", "lane-1"),
            ("planned", "lane-1"),
            ("planned", "lane-2"),
            ("planned", "lane-2"),
            ("planned", "lane-3"),
            ("done", "lane-1"),
            ("done", "lane-1"),
            ("done", "lane-1"),
            ("done", "lane-2"),
            ("done", "lane-2"),
            ("done", "lane-3"),
        ];
        for (index, (status, assignee)) in SEED.iter().enumerate() {
            let card_notes = if index == 0 {
                format!("{notes} needle")
            } else {
                notes.clone()
            };
            let mut create = json!({
                "action":"create",
                "title":format!("probe card {index}"),
                "status":status,
                "notes":card_notes,
            });
            if !assignee.is_empty() {
                create["assignee"] = json!(assignee);
            }
            let actor = if assignee.is_empty() {
                "lead"
            } else {
                assignee
            };
            payload(&tool(&home, id, actor, "cccc_task", create).await);
        }

        let all = tool(&home, id, "peer", "cccc_task", json!({"action":"list"})).await;
        let all = payload(&all)["tasks"].as_array().expect("tasks").to_vec();
        assert_eq!(all.len(), 12, "an unfiltered list returns the whole board");
        let board_bytes = bytes(&json!(all));

        // The reported defect: a status-filtered call returned everything.
        let planned = tool(
            &home,
            id,
            "peer",
            "cccc_task",
            json!({"action":"list","status":"planned"}),
        )
        .await;
        let planned = payload(&planned)["tasks"]
            .as_array()
            .expect("tasks")
            .to_vec();
        assert_eq!(
            planned.len(),
            6,
            "status=planned must reach the daemon filter"
        );
        assert!(
            planned.iter().all(|task| task["status"] == "planned"),
            "a filtered list returned a card outside the filter"
        );
        assert!(
            bytes(&json!(planned)) < board_bytes,
            "a filtered list must not carry the cards it filtered out: planned={} board={}",
            bytes(&json!(planned)),
            board_bytes
        );

        let unassigned = tool(
            &home,
            id,
            "peer",
            "cccc_task",
            json!({"action":"list","attention":"unassigned"}),
        )
        .await;
        let unassigned = payload(&unassigned)["tasks"]
            .as_array()
            .expect("tasks")
            .to_vec();
        assert_eq!(unassigned.len(), 1, "attention=unassigned");
        assert_eq!(unassigned[0]["id"], "T001");
        assert!(
            bytes(&json!(unassigned)) * 6 < board_bytes,
            "one card must not cost the board"
        );

        let lane = tool(
            &home,
            id,
            "peer",
            "cccc_task",
            json!({"action":"list","assignee":"lane-1"}),
        )
        .await;
        let lane = payload(&lane)["tasks"].as_array().expect("tasks").to_vec();
        assert_eq!(lane.len(), 5, "assignee=lane-1 across statuses");
        assert!(lane.iter().all(|task| task["assignee"] == "lane-1"));

        let searched = tool(
            &home,
            id,
            "peer",
            "cccc_task",
            json!({"action":"list","query":"needle"}),
        )
        .await;
        let found = payload(&searched)["tasks"]
            .as_array()
            .expect("tasks")
            .to_vec();
        assert_eq!(found.len(), 1, "query matches notes");
        assert_eq!(found[0]["id"], "T001");

        let first = tool(
            &home,
            id,
            "peer",
            "cccc_task",
            json!({"action":"list","limit":4,"offset":0}),
        )
        .await;
        let first = payload(&first).clone();
        assert_eq!(first["count"], 4);
        assert_eq!(first["total_count"], 12);
        assert_eq!(first["limit"], 4);
        assert_eq!(first["has_more"], true);
        let second = tool(
            &home,
            id,
            "peer",
            "cccc_task",
            json!({"action":"list","limit":4,"offset":8}),
        )
        .await;
        let second = payload(&second).clone();
        assert_eq!(second["count"], 4);
        assert_eq!(second["offset"], 8);
        assert_eq!(second["has_more"], false);
        let paged_ids = |page: &Value| {
            page["tasks"]
                .as_array()
                .expect("tasks")
                .iter()
                .map(|task| task["id"].as_str().expect("id").to_owned())
                .collect::<Vec<_>>()
        };
        let overlap = paged_ids(&first)
            .into_iter()
            .filter(|id| paged_ids(&second).contains(id))
            .count();
        assert_eq!(overlap, 0, "offset must move the window");
        assert!(
            bytes(&first) * 2 < board_bytes,
            "a page must not carry the board"
        );

        let pages = tool(
            &home,
            id,
            "peer",
            "cccc_task",
            json!({"action":"list","statuses":"planned,done","limit":3}),
        )
        .await;
        let pages = payload(&pages).clone();
        assert_eq!(pages["pages"]["planned"]["count"], 3);
        assert_eq!(pages["pages"]["planned"]["total_count"], 6);
        assert_eq!(pages["pages"]["done"]["count"], 3);
        assert_eq!(pages["pages"]["done"]["total_count"], 6);

        let indexed = tool(
            &home,
            id,
            "peer",
            "cccc_task",
            json!({"action":"list","limit":2,"include_index":true}),
        )
        .await;
        assert_eq!(
            payload(&indexed)["task_index"]
                .as_array()
                .expect("index")
                .len(),
            12
        );

        // The op's validation is reachable now: a bad filter fails loudly
        // instead of being dropped on the floor.
        let bogus = tool(
            &home,
            id,
            "peer",
            "cccc_task",
            json!({"action":"list","status":"bogus"}),
        )
        .await;
        assert_eq!(bogus["result"]["isError"], true, "{bogus}");
        assert_eq!(error_code(&bogus), "invalid_args");
        let zero = tool(
            &home,
            id,
            "peer",
            "cccc_task",
            json!({"action":"list","limit":0}),
        )
        .await;
        assert_eq!(error_code(&zero), "invalid_args");
        let dangling_offset = tool(
            &home,
            id,
            "peer",
            "cccc_task",
            json!({"action":"list","offset":5}),
        )
        .await;
        assert_eq!(error_code(&dangling_offset), "invalid_args");

        // A single-card read stays narrow and unchanged.
        let single = tool(
            &home,
            id,
            "peer",
            "cccc_task",
            json!({"action":"list","task_id":"T001"}),
        )
        .await;
        assert_eq!(payload(&single)["task"]["id"], "T001");
    })
    .await;
    daemon_task.abort();
    let _ = daemon_task.await;
    outcome.expect("task list filter assertions");
}
