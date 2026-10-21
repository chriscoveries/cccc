#![cfg(unix)]

//! End-to-end PTY liveness: a real PTY child prints once and then goes quiet,
//! and `actor_list` must report `working` while that output is inside the
//! active window and `idle` once it ages out. On `main` the same assertions
//! fail with `waiting` / `pty_running_state_unknown` for the whole life of the
//! process, which is exactly the dashboard defect this covers.
//!
//! The production window (30 s) and byte floor (2048 B) are used as shipped, so
//! the run takes about 40 s: a shorter window would mean a test-only code path,
//! and the point is that a real child crosses the real threshold.

use cccc_contracts::{DaemonRequest, DaemonResponse};
use cccc_core::HomeLayout;
use serde_json::{Map, Value, json};

fn call(home: &HomeLayout, op: &str, args: Value) -> DaemonResponse {
    let request = DaemonRequest {
        v: 1,
        op: op.into(),
        args: args.as_object().cloned().unwrap_or_else(Map::new),
    };
    let response = cccc_daemon::handle_request(home, &request);
    assert!(
        response.ok,
        "{op} failed: {:?}",
        response.error.as_ref().map(|error| &error.message)
    );
    response
}

fn actor_fields(home: &HomeLayout, group_id: &str, actor_id: &str) -> Value {
    let listed = call(home, "actor_list", json!({"group_id":group_id,"by":"user"}));
    listed.result["actors"]
        .as_array()
        .and_then(|actors| actors.iter().find(|actor| actor["id"] == actor_id))
        .cloned()
        .unwrap_or_else(|| panic!("{actor_id} missing from actor_list: {:?}", listed.result))
}

fn poll_until(
    home: &HomeLayout,
    group_id: &str,
    actor_id: &str,
    deadline: std::time::Duration,
    check: impl Fn(&Value) -> bool,
) -> Value {
    let start = std::time::Instant::now();
    loop {
        let fields = actor_fields(home, group_id, actor_id);
        if check(&fields) {
            return fields;
        }
        if start.elapsed() >= deadline {
            return fields;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[test]
fn printing_pty_child_reads_working_then_idle() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = HomeLayout::from_path(temp.path().join("rust-home")).expect("home");
    home.initialize().expect("initialize");
    let created = call(
        &home,
        "group_create",
        json!({"title":"pty-working-state","by":"user"}),
    );
    let group_id = created.result["group"]["group_id"]
        .as_str()
        .expect("group id")
        .to_owned();
    // The PTY runner needs an active scope, the same one a real lane runs in.
    let project = temp.path().join("project");
    std::fs::create_dir(&project).expect("project scope");
    call(
        &home,
        "attach",
        json!({"group_id":group_id,"path":project,"by":"user"}),
    );
    // A real terminal process that prints one 3 KiB banner and then says
    // nothing for good: those are the only bytes it will ever emit.
    let banner = "x".repeat(3072);
    assert!(
        call(
            &home,
            "actor_add",
            json!({
                "group_id":group_id,
                "actor_id":"pty1",
                "runtime":"custom",
                "command":["sh","-c",format!("printf '%s' '{banner}'; printf 'ready> '; sleep 90")],
                "by":"user"
            }),
        )
        .ok
    );
    call(
        &home,
        "actor_start",
        json!({"group_id":group_id,"actor_id":"pty1","by":"user"}),
    );

    let working = poll_until(
        &home,
        &group_id,
        "pty1",
        std::time::Duration::from_secs(20),
        |fields| fields["effective_working_state"] == "working",
    );
    assert_eq!(working["effective_working_state"], "working", "{working}");
    assert_eq!(working["effective_working_reason"], "pty_output_active");
    assert!(
        working["idle_seconds"].is_u64(),
        "idle_seconds must be a number once output has been observed: {working}"
    );
    assert!(
        working["effective_working_updated_at"].is_string(),
        "the last-output timestamp must be reported: {working}"
    );

    // The child is still running and still silent; past the window it is idle.
    let idle = poll_until(
        &home,
        &group_id,
        "pty1",
        std::time::Duration::from_secs(60),
        |fields| fields["effective_working_state"] == "idle",
    );
    assert_eq!(idle["effective_working_state"], "idle", "{idle}");
    assert_eq!(idle["effective_working_reason"], "pty_output_quiet");
    assert!(
        idle["idle_seconds"].as_u64().unwrap_or(0) >= 30,
        "idle_seconds must count from the last output: {idle}"
    );
    assert_eq!(idle["running"], true, "the process is still alive: {idle}");

    call(
        &home,
        "actor_stop",
        json!({"group_id":group_id,"actor_id":"pty1","by":"user"}),
    );
}
