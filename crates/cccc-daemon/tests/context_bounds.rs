// Bounded `context_get` through the real daemon op.
//
// The unit tests next to the projection prove the paging and the byte cap on a
// verbatim capture of a real 775-card group. This test proves the op wiring:
// the default answer is bounded, a cursor walks the whole board, group.yaml
// caps are honoured, and `unbounded` still returns the legacy full snapshot.
use cccc_contracts::DaemonRequest;
use cccc_core::{GroupStore, HomeLayout};
use serde_json::{Value, json};

fn call(home: &HomeLayout, op: &str, args: Value) -> Value {
    let response = cccc_daemon::handle_request(
        home,
        &DaemonRequest {
            v: 1,
            op: op.into(),
            args: args.as_object().expect("object args").clone(),
        },
    );
    assert!(response.ok, "{op}: {:?}", response.error);
    Value::Object(response.result)
}

fn error_code(home: &HomeLayout, op: &str, args: Value) -> Option<String> {
    let response = cccc_daemon::handle_request(
        home,
        &DaemonRequest {
            v: 1,
            op: op.into(),
            args: args.as_object().cloned().unwrap_or_default(),
        },
    );
    response.error.map(|error| error.code)
}

fn size(value: &Value) -> usize {
    serde_json::to_vec(value).expect("serialized").len()
}

fn page_ids(context: &Value) -> Vec<String> {
    context
        .pointer("/coordination/tasks")
        .and_then(Value::as_array)
        .expect("task page")
        .iter()
        .filter_map(|task| task.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect()
}

#[test]
fn context_get_is_bounded_by_default_and_pages_the_whole_board() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    home.initialize().expect("initialize");
    let store = GroupStore::new(home.clone()).expect("store");
    let group = store.create("bounded", "").expect("group");
    let group_id = group.group_id.clone();

    for index in 0..130 {
        let title = format!("card-{index:03}");
        let created_at = call(
            &home,
            "context_sync",
            json!({
                "group_id":group_id,
                "by":"user",
                "ops":[{"op":"task.create","title":title}]
            }),
        );
        assert_eq!(created_at["success"], json!(true));
    }
    let listed = call(&home, "task_list", json!({"group_id":group_id,"by":"user"}));
    let total = listed["tasks"].as_array().expect("tasks").len();
    assert_eq!(total, 130);

    let first = call(&home, "context_get", json!({"group_id":group_id}));
    assert_eq!(first["truncated"], json!(true));
    assert_eq!(first["paging"]["task_total"], json!(130));
    assert!(page_ids(&first).len() <= 100);
    assert!(size(&first) <= 256 * 1024);
    let cursor = first["next_cursor"]
        .as_str()
        .expect("next cursor")
        .to_owned();

    let mut seen = page_ids(&first);
    let mut next = Some(cursor);
    let mut pages = 1;
    while let Some(cursor) = next {
        let page = call(
            &home,
            "context_get",
            json!({"group_id":group_id,"cursor":cursor}),
        );
        seen.extend(page_ids(&page));
        next = page["next_cursor"].as_str().map(str::to_owned);
        pages += 1;
        assert!(pages < 20, "paging must terminate");
    }
    assert!(pages > 1);
    assert_eq!(seen.len(), 130);
    let mut deduped = seen.clone();
    deduped.sort();
    deduped.dedup();
    assert_eq!(deduped.len(), 130);

    let unbounded = call(
        &home,
        "context_get",
        json!({"group_id":group_id,"unbounded":true}),
    );
    // The unbounded path returns the legacy projection verbatim: no paging
    // fields, no byte cap, every card present.
    assert_eq!(page_ids(&unbounded).len(), 130);
    assert!(unbounded.get("truncated").is_none());
    assert!(unbounded.get("next_cursor").is_none());
    assert!(unbounded.get("paging").is_none());

    let mut group = store.load(&group_id).expect("reload");
    group.extra.insert("max_board_items".into(), json!(7));
    store.save(&group).expect("save caps");
    let capped = call(&home, "context_get", json!({"group_id":group_id}));
    assert!(page_ids(&capped).len() <= 7);
    assert_eq!(capped["truncated"], json!(true));

    let mut group = store.load(&group_id).expect("reload");
    group.extra.insert("state_max_bytes".into(), json!(2048));
    store.save(&group).expect("save caps");
    let tiny = call(&home, "context_get", json!({"group_id":group_id}));
    assert!(size(&tiny) <= 2048, "{}", size(&tiny));
}

#[test]
fn malformed_context_arguments_fail_once_with_invalid_args() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    home.initialize().expect("initialize");
    let group_id = GroupStore::new(home.clone())
        .expect("store")
        .create("bounded", "")
        .expect("group")
        .group_id;

    for args in [
        json!({"group_id":group_id,"cursor":"nonsense"}),
        json!({"group_id":group_id,"include":["board","everything"]}),
        json!({"group_id":group_id,"limit":-3}),
    ] {
        assert_eq!(
            error_code(&home, "context_get", args.clone()).as_deref(),
            Some("invalid_args"),
            "{args}"
        );
    }
}
