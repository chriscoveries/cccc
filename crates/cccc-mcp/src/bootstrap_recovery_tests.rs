//! Tests-only checkpoint: bootstrap recovery on oversized full context.
//!
//! WIRING (root applies — UNWIRED; no existing source touched):
//! ```ignore
//! #[cfg(test)]
//! #[path = "bootstrap_recovery_tests.rs"]
//! mod recovery_tests;
//! ```
//! in `crates/cccc-mcp/src/bootstrap.rs` (top level, so `super::build` and
//! private helpers resolve). NOT compiled or run here — predictions below;
//! root names actual RED.
//!
//! CONTRACT UNDER TEST (this file DEFINES it, tests-first): when the daemon
//! answers default-full `context_get` with typed `context_too_large,
//! `build()` must NOT abort. It falls back to exactly one
//! `context_get` with `detail="overview"`, builds the recovery pack from
//! that projection, and marks the pack explicitly:
//! `tasks.full_context_unavailable == true` with
//! `tasks.full_context_via == "task_list"` (flows into
//! `recovery.task_slice`). Any other daemon error propagates unchanged; no
//! silent fresh, no dropped required constraints.
//!
//! DEGRADATION (exact, from source): overview carries version,
//! tasks_version, full brief/decisions/handoffs, full agent_states, meta —
//! but NO coordination.tasks/attention/board/summary. So the recovered pack
//! keeps agent identity, brief, and notes, while assigned_active/attention
//! are empty BY CONSTRUCTION. The marker is what keeps that honest instead
//! of looking like "no work". Overview itself is unbounded via meta/agent
//! states — out of this contract's scope (noted, not tested).
//!
//! HARNESS (no fake in-tree: `DaemonClient` has no seam and `lib_tests.rs`
//! only covers the real-daemon direction): a scripted unix-socket fake
//! daemon answering per-op canned responses from the test temp home's
//! `ccccd.addr.json`. Scripted by REQUEST (op + detail arg), never by
//! desired outcome. Real `DaemonClient`, real `super::build`, real JSON
//! line protocol both ways.
//!
//! PREDICTIONS on 2696 (no fallback implemented): `oversized_full_falls_
//! back_to_marked_overview` FAILS (typed error aborts `build` — the exact
//! hazard); `unrelated_daemon_errors_propagate` and
//! `required_args_preserved` PASS now and must stay green.

use super::*;
use cccc_client::DaemonClient;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Proposed bound: 64 KiB serialized bootstrap payload. The overview-derived
/// pack is token-slimmed (~1–2 KiB) plus hygiene, empty inbox, and session;
/// 64 KiB is 10×+ headroom while catching MB-scale regressions.
const RECOVERY_BUDGET_BYTES: usize = 64 * 1024;

const ACTOR: &str = "peer1";

/// Scripted fake daemon. `routes`: op -> handler over request args.
/// Every exchange is logged as `op/detail-or--` for fallback proof.
type Handler = Arc<dyn Fn(&Map<String, Value>) -> Value + Send + Sync>;

struct FakeDaemon {
    _temp: tempfile::TempDir,
    log: Arc<Mutex<Vec<String>>>,
    _task: tokio::task::JoinHandle<()>,
}

impl FakeDaemon {
    async fn start(routes: HashMap<String, Handler>) -> (Self, HomeLayout, DaemonClient) {
        let temp = tempfile::tempdir().expect("fake home");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let sock = temp.path().join("fake-daemon.sock");
        let listener = tokio::net::UnixListener::bind(&sock).expect("bind fake socket");
        let addr = json!({
            "v": 1, "transport": "unix", "path": sock.to_string_lossy(),
            "host": "", "port": 0, "pid": 1,
            "version": "fake", "ts": "2026-10-16T00:00:00Z",
        });
        let dir = home.daemon_dir();
        std::fs::create_dir_all(&dir).expect("daemon dir");
        std::fs::write(dir.join("ccccd.addr.json"), serde_json::to_vec(&addr).expect("addr"))
            .expect("addr file");
        let log = Arc::new(Mutex::new(Vec::new()));
        let routes = Arc::new(routes);
        let task_log = log.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let routes = routes.clone();
                let task_log = task_log.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
                    let mut reader = BufReader::new(stream);
                    loop {
                        let mut line = String::new();
                        match reader.read_line(&mut line).await {
                            Ok(0) | Err(_) => return,
                            Ok(_) => {}
                        }
                        let request: Value = serde_json::from_str(&line).unwrap_or(json!({}));
                        let op = request
                            .get("op")
                            .and_then(Value::as_str)
                            .unwrap_or("?")
                            .to_owned();
                        let detail = request
                            .pointer("/args/detail")
                            .and_then(Value::as_str)
                            .unwrap_or("-")
                            .to_owned();
                        task_log.lock().expect("log").push(format!("{op}/{detail}"));
                        let response = routes
                            .get(&op)
                            .map(|handler| handler(request.get("args").and_then(Value::as_object)))
                            .unwrap_or_else(|| {
                                json!({"v":1,"ok":false,"error":{"code":"unknown_op","message":"fake has no route"}})
                            });
                        let mut out = serde_json::to_vec(&response).expect("response");
                        out.push(b'\n');
                        let stream = reader.get_mut();
                        if stream.write_all(&out).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        let client = DaemonClient::new(home.clone());
        (Self { _temp: temp, log, _task: task }, home, client)
    }

    fn ops(&self) -> Vec<String> {
        self.log.lock().expect("log").clone()
    }
}

fn ok(payload: Value) -> Value {
    // DaemonResponse envelope: the daemon result object nests under
    // "result" — a top-level payload without it parses as an EMPTY result.
    let mut envelope = Map::from_iter([("v".into(), json!(1)), ("ok".into(), json!(true))]);
    envelope.insert(
        "result".into(),
        payload.as_object().cloned().map(Value::Object).unwrap_or(payload),
    );
    Value::Object(envelope)
}

fn overview_doc(actor: &str) -> Map<String, Value> {
    json!({
        "version": "v-test",
        "tasks_version": "tasksv:9",
        "coordination": {
            "brief": {"objective": "stay small", "current_focus": "recovery"},
            "recent_decisions": [],
            "recent_handoffs": []
        },
        "agent_states": [{
            "id": actor,
            "hot": {"focus": "survive oversized context", "next_action": "read task_list"},
            "warm": {"what_changed": "seeded", "environment_summary": "fake",
                     "user_model": "u", "persona_notes": "p"},
            "updated_at": "2026-10-16T00:00:00Z"
        }],
        "actors_runtime": [],
        "meta": {}
    })
    .as_object()
    .cloned()
    .expect("overview")
}

fn args(home: &HomeLayout, group: &str) -> Map<String, Value> {
    let _ = home;
    Map::from_iter([
        ("group_id".into(), json!(group)),
        ("actor_id".into(), json!(ACTOR)),
        ("inbox_limit".into(), json!(1)),
    ])
}

#[tokio::test]
async fn oversized_full_falls_back_to_marked_overview() {
    // PREDICTION: RED on 2696 — the typed error aborts build() (the hazard).
    // Post-fix: full→typed-error, then overview→marked small pack.
    // NOTE: the overview route below serves the REAL overview doc shape
    // (see `overview_doc`); assertions verify its content flows through to
    // the payload, so a stubbed pack cannot pass.
    let mut routes: HashMap<String, Handler> = HashMap::new();
    routes.insert("group_show".into(), Arc::new(|_| ok(json!({"group": {"id": "g1"}}))));
    routes.insert("actor_list".into(), Arc::new(|_| ok(json!({"actors": []}))));
    let overview = Value::Object(overview_doc(ACTOR));
    routes.insert(
        "context_get".into(),
        Arc::new(move |args| {
            if args.get("detail").and_then(Value::as_str) == Some("overview") {
                ok(overview.clone())
            } else {
                json!({"v":1,"ok":false,"error":{"code":"context_too_large",
                    "message":"full context exceeds 3145728 serialized bytes; use detail=overview plus task_list",
                    "details":{}}})
            }
        }),
    );
    routes.insert("inbox_peek".into(), Arc::new(|_| ok(json!({"messages": []}))));
    routes.insert("memory_reme_search".into(), Arc::new(|_| ok(json!({"hits": []}))));
    let (fake, home, client) = FakeDaemon::start(routes).await;
    let (fake, home, client) = FakeDaemon::start(routes).await;
    let payload = super::build(&home, &client, args(&home, "g1"))
        .await
        .expect("bootstrap must not abort on oversized full context");
    let ops = fake.ops();
    assert!(
        ops.iter().any(|op| op == "context_get/-")
            && ops.iter().any(|op| op == "context_get/overview"),
        "fallback must read overview after the typed full error, saw: {ops:?}"
    );
    let slice = &payload["recovery"]["task_slice"];
    assert_eq!(slice["full_context_unavailable"], true, "explicit marker, never silent");
    assert_eq!(slice["full_context_via"], "task_list");
    assert_eq!(slice["assigned_active"], json!([]));
    assert_eq!(slice["attention"], json!([]));
    assert_eq!(
        payload["recovery"]["self_state"]["hot"]["focus"],
        "survive oversized context",
        "overview content must actually flow through, not a stubbed pack"
    );
    let bytes = serde_json::to_vec(&payload).map(|b| b.len()).unwrap_or(usize::MAX);
    assert!(bytes <= RECOVERY_BUDGET_BYTES, "recovery payload {bytes} bytes over budget");
}

#[tokio::test]
async fn unrelated_daemon_errors_propagate_without_fallback() {
    // PREDICTION: GREEN on 2696 and after — only the typed oversized error
    // takes the recovery path; every other daemon failure still aborts.
    // (Also proves the fake harness itself fails loudly, not vacuously.)
    let mut routes: HashMap<String, Handler> = HashMap::new();
    routes.insert(
        "group_show".into(),
        Arc::new(|_| {
            json!({"v":1,"ok":false,"error":{"code":"group_not_found",
                "message":"no such group","details":{}}})
        }),
    );
    let (fake, home, client) = FakeDaemon::start(routes).await;
    let error = super::build(&home, &client, args(&home, "g-missing"))
        .await
        .expect_err("unrelated daemon errors must abort bootstrap");
    assert!(error.to_string().contains("group_not_found"), "code propagates: {error}");
    assert!(
        !fake.ops().iter().any(|op| op == "context_get/overview"),
        "no recovery read on unrelated errors, saw: {:?}",
        fake.ops()
    );
}

#[tokio::test]
async fn required_args_preserved_without_silent_fresh() {
    // PREDICTION: GREEN on 2696 and after — required constraints are never
    // dropped to manufacture a payload.
    let (_fake, home, client) = FakeDaemon::start(HashMap::new()).await;
    let error = super::build(&home, &client, Map::new())
        .await
        .expect_err("missing group_id must fail, not fresh-start");
    assert!(error.to_string().contains("group_id"), "constraint named: {error}");
}
