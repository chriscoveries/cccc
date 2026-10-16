//! Tests-only checkpoint v2: bootstrap recovery on oversized full context.
//!
//! Supersedes the v1 checkpoint file (preserved in git history, NOT amended)
//! after review defects: Option-vs-Map handler mismatch, a duplicated
//! `start` call, missing unix gating, weak unrelated-error control,
//! op-string-only logging, unseeded omission semantics, unbounded client
//! timeout, and a detached listener task.
//!
//! WIRING (root applies — UNWIRED; no existing source touched):
//! ```ignore
//! #[cfg(all(test, target_os = "unix"))]
//! #[path = "bootstrap_recovery_tests.rs"]
//! mod recovery_tests;
//! ```
//! in `crates/cccc-mcp/src/bootstrap.rs` (top level; unix-gated for
//! `UnixListener`). NOT compiled or run here — predictions below; root
//! names actual RED.
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
//! SIZE NOTE: 64 KiB below is a FIXTURE ceiling for these tests only, not a
//! new global policy. (MCP residual, out of scope here — H3 maps separately:
//! `router::tool_result` pretty text + structuredContent copies the payload,
//! so a daemon-side Map bound is not an SDK-envelope proof.)
//!
//! DEGRADATION (exact, from source): overview carries version,
//! tasks_version, full brief/decisions/handoffs, full agent_states, meta —
//! but NO coordination.tasks/attention/board/summary. The recovered pack
//! keeps agent identity, brief, and notes (all asserted flowing through),
//! while assigned_active/attention are empty BY CONSTRUCTION. The marker is
//! what keeps that honest instead of looking like "no work".
//!
//! PREDICTIONS on 2696 (no fallback implemented): `oversized_full_falls_
//! back_to_marked_overview` FAILS (typed error aborts `build` — the exact
//! hazard); the other two PASS now and must stay green.

use super::*;
use cccc_client::DaemonClient;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Fixture ceiling for THESE tests only (see header): the overview-derived
/// pack is token-slimmed (~1–2 KiB) plus hygiene, tiny inbox, and session.
const RECOVERY_BUDGET_BYTES: usize = 64 * 1024;

const ACTOR: &str = "peer1";

/// Scripted fake daemon. `routes`: op -> handler over VALIDATED request args.
/// Every exchange records the FULL request (op + args snapshot) so tests
/// assert exact sequences and preserved arguments, not op strings.
type Handler = Arc<dyn Fn(&Map<String, Value>) -> Value + Send + Sync>;

struct FakeDaemon {
    _temp: tempfile::TempDir,
    log: Arc<Mutex<Vec<Value>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeDaemon {
    fn drop(&mut self) {
        // Never leak the listener past the test: abort, do not join (a
        // stuck accept must not hang teardown).
        self.task.abort();
    }
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
        std::fs::write(
            dir.join("ccccd.addr.json"),
            serde_json::to_vec(&addr).expect("addr"),
        )
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
                        // Validate args BEFORE any handler call: handlers
                        // take &Map, never Option. Malformed requests get a
                        // typed fake-side error, never a handler panic.
                        let Some(args) = request.get("args").and_then(Value::as_object) else {
                            let mut out = serde_json::to_vec(&json!({"v":1,"ok":false,
                                "error":{"code":"fake_malformed_request",
                                "message":"args object required"}}))
                            .expect("error");
                            out.push(b'\n');
                            let stream = reader.get_mut();
                            if stream.write_all(&out).await.is_err() {
                                return;
                            }
                            continue;
                        };
                        let op = request
                            .get("op")
                            .and_then(Value::as_str)
                            .unwrap_or("?")
                            .to_owned();
                        task_log.lock().expect("log").push(json!({
                            "op": op,
                            "args": args,
                        }));
                        let response = routes
                            .get(&op)
                            .map(|handler| handler(args))
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
        // Bound the client well below the 60 s default: a silent fake must
        // fail the test in seconds, never hang the suite.
        let client = DaemonClient::new(home.clone()).with_timeout(Duration::from_secs(10));
        (
            Self {
                _temp: temp,
                log,
                task,
            },
            home,
            client,
        )
    }

    fn requests(&self) -> Vec<Value> {
        self.log.lock().expect("log").clone()
    }

    fn ops(&self) -> Vec<String> {
        self.requests()
            .into_iter()
            .filter_map(|entry| {
                entry.get("op").and_then(Value::as_str).map(|op| {
                    let detail = entry
                        .pointer("/args/detail")
                        .and_then(Value::as_str)
                        .unwrap_or("-");
                    format!("{op}/{detail}")
                })
            })
            .collect()
    }
}

fn ok(payload: Value) -> Value {
    // DaemonResponse envelope: the daemon result object nests under
    // "result" — a top-level payload without it parses as an EMPTY result.
    let mut envelope = Map::from_iter([("v".into(), json!(1)), ("ok".into(), json!(true))]);
    envelope.insert(
        "result".into(),
        payload
            .as_object()
            .cloned()
            .map(Value::Object)
            .unwrap_or(payload),
    );
    Value::Object(envelope)
}

fn overview_doc(actor: &str) -> Map<String, Value> {
    json!({
        "version": "v-test",
        "tasks_version": "tasksv:9",
        "coordination": {
            "brief": {"objective": "stay small", "current_focus": "recovery"},
            "recent_decisions": [{"summary": "seeded decision"}],
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

fn args(group: &str) -> Map<String, Value> {
    Map::from_iter([
        ("group_id".into(), json!(group)),
        ("actor_id".into(), json!(ACTOR)),
        ("inbox_limit".into(), json!(1)),
    ])
}

fn context_sequence(log: &[Value]) -> Vec<String> {
    log.iter()
        .filter(|entry| entry.get("op").and_then(Value::as_str) == Some("context_get"))
        .map(|entry| {
            entry
                .pointer("/args/detail")
                .and_then(Value::as_str)
                .unwrap_or("(default-full)")
                .to_owned()
        })
        .collect()
}

#[tokio::test]
async fn oversized_full_falls_back_to_marked_overview() {
    // PREDICTION: RED on 2696 — the typed error aborts build() (the hazard).
    // Post-fix: full→typed-error, then exactly one overview→marked small pack.
    let mut routes: HashMap<String, Handler> = HashMap::new();
    routes.insert(
        "group_show".into(),
        Arc::new(|_| ok(json!({"group": {"id": "g1"}}))),
    );
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
    routes.insert(
        "inbox_peek".into(),
        Arc::new(|_| ok(json!({"messages": []}))),
    );
    routes.insert(
        "memory_reme_search".into(),
        Arc::new(|_| ok(json!({"hits": []}))),
    );
    let (fake, home, client) = FakeDaemon::start(routes).await;
    let payload = super::build(&home, &client, args("g1"))
        .await
        .expect("bootstrap must not abort on oversized full context");
    // Exact fallback sequence AND preserved call arguments (group/actor/by),
    // not op strings alone.
    let sequence = context_sequence(&fake.requests());
    assert_eq!(
        sequence,
        vec!["(default-full)".to_owned(), "overview".to_owned()],
        "exactly one full attempt then exactly one overview read, saw: {sequence:?}"
    );
    for entry in fake.requests() {
        if entry.get("op").and_then(Value::as_str) == Some("context_get") {
            assert_eq!(
                entry.pointer("/args/group_id").and_then(Value::as_str),
                Some("g1")
            );
            assert_eq!(
                entry.pointer("/args/actor_id").and_then(Value::as_str),
                Some(ACTOR)
            );
        }
    }
    let slice = &payload["recovery"]["task_slice"];
    assert_eq!(
        slice["full_context_unavailable"], true,
        "explicit marker, never silent"
    );
    assert_eq!(slice["full_context_via"], "task_list");
    assert_eq!(slice["assigned_active"], json!([]));
    assert_eq!(slice["attention"], json!([]));
    // Seeded coordination content flows through AND omission semantics hold:
    // decisions present (overview carries them), task slice empty-but-marked.
    assert_eq!(
        payload["recovery"]["self_state"]["hot"]["focus"], "survive oversized context",
        "overview agent content must flow through, not a stubbed pack"
    );
    assert_eq!(
        payload["recovery"]["recent_notes"]["decisions"][0]["summary"], "seeded decision",
        "seeded coordination note must flow through"
    );
    let bytes = serde_json::to_vec(&payload)
        .map(|b| b.len())
        .unwrap_or(usize::MAX);
    assert!(
        bytes <= RECOVERY_BUDGET_BYTES,
        "recovery payload {bytes} bytes over budget"
    );
}

#[tokio::test]
async fn unrelated_daemon_errors_propagate_without_fallback() {
    // PREDICTION: GREEN on 2696 and after — the fallback triggers ONLY on
    // the typed oversized error, at context_get, after group_show+actor_list
    // already succeeded. (Failing at group_show would never reach context_get
    // and therefore proves nothing about the fallback gate.)
    let mut routes: HashMap<String, Handler> = HashMap::new();
    routes.insert(
        "group_show".into(),
        Arc::new(|_| ok(json!({"group": {"id": "g1"}}))),
    );
    routes.insert("actor_list".into(), Arc::new(|_| ok(json!({"actors": []}))));
    routes.insert(
        "context_get".into(),
        Arc::new(|_| {
            json!({"v":1,"ok":false,"error":{"code":"store_unavailable",
                "message":"state store unreachable","details":{"retryable":true}}})
        }),
    );
    let (fake, home, client) = FakeDaemon::start(routes).await;
    let error = super::build(&home, &client, args("g1"))
        .await
        .expect_err("unrelated daemon errors must abort bootstrap");
    // Exact code+message via Display ("code: message"); details preservation
    // is by construction (`router::daemon` maps DaemonError code/message/
    // details whole into ToolCallError — verified in source, not re-asserted
    // here since the fields are crate-private to the MCP root).
    assert_eq!(
        error.to_string(),
        "store_unavailable: state store unreachable",
        "exact error propagates: {error}"
    );
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
    assert!(
        error.to_string().contains("group_id"),
        "constraint named: {error}"
    );
}
