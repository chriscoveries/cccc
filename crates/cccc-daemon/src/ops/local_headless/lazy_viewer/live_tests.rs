//! Real native Kilo, its ACP bridge and a loopback model. No provider credentials.
use super::super::{BatchSubmission, Session, supervisor};
use cccc_contracts::{Actor, ActorRuntime, DaemonRequest, Event};
use cccc_core::{GroupDoc, GroupStore, HomeLayout, ledger};
use futures_util::FutureExt;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

#[derive(Default)]
struct ModelRequests {
    primary: AtomicUsize,
    auxiliary: AtomicUsize,
    unexpected: AtomicUsize,
    routes: Mutex<Vec<Value>>,
}

impl ModelRequests {
    fn observe(&self, body: &Value) {
        let (role, counter) = match body["model"].as_str() {
            Some("first") => ("primary", &self.primary),
            Some("auxiliary") => ("auxiliary", &self.auxiliary),
            _ => ("unexpected", &self.unexpected),
        };
        let ordinal = counter.fetch_add(1, Ordering::AcqRel) + 1;
        let mut routes = self.routes.lock().expect("request observations");
        if routes.len() < 32 {
            routes.push(json!({"role":role,"ordinal":ordinal}));
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_native_lazy_viewer_when_enabled() {
    if std::env::var("CCCC_KILO_MANAGED_LIVE").as_deref() != Ok("1") {
        return;
    }
    let requests = Arc::new(ModelRequests::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("local model");
    let endpoint = format!("http://{}/v1", listener.local_addr().expect("address"));
    let count = Arc::clone(&requests);
    let model = tokio::spawn(async move {
        axum::serve(
            listener,
            axum::Router::new().route(
                "/v1/chat/completions",
                axum::routing::post(move |body: axum::Json<Value>| {
                    let count = Arc::clone(&count);
                    async move {
                        // Native title generation uses small_model and is not a
                        // second delivery. Keep the primary-turn oracle strict.
                        count.observe(&body);
                        crate::ops::codex_voice_analyst::lazy_viewer_test_model(body).await
                    }
                }),
            ),
        )
        .await
        .expect("model server");
    });
    let result = std::panic::AssertUnwindSafe(async {
        let mut samples = Vec::new();
        for enabled in [false, true] {
            let temp = tempfile::tempdir().expect("isolated native home");
            let root = temp.path();
            let project = root.join("project");
            std::fs::create_dir(&project).expect("project");
            let home = HomeLayout::from_path(root.join("cccc")).expect("home");
            home.initialize().expect("initialize");
            let store = GroupStore::new(home.clone()).expect("store");
            let mut group = store.create("native viewer probe", "").expect("group");
            let mut actor = Actor::new("probe");
            actor.runtime = ActorRuntime::Kilo;
            if enabled { actor.env.insert("CCCC_OPENCODE_HEADLESS".into(), "1".into()); }
            let mut env = BTreeMap::from([
                ("HOME".into(), root.to_string_lossy().into_owned()),
                ("KILO_DB".into(), root.join("kilo.db").to_string_lossy().into_owned()),
                ("TERM".into(), "xterm-256color".into()),
                ("KILO_CONFIG_CONTENT".into(), json!({"model":"cccc-test/first","small_model":"cccc-test/auxiliary",
                    "provider":{"cccc-test":{"npm":"@ai-sdk/openai-compatible",
                        "options":{"baseURL":endpoint,"apiKey":"local-test"},
                        "models":{"first":{"name":"First"},"auxiliary":{"name":"Auxiliary"}}}}}).to_string()),
            ]);
            for part in ["CONFIG", "DATA", "STATE", "CACHE"] {
                env.insert(format!("XDG_{part}_HOME"), root.join(part.to_lowercase()).to_string_lossy().into_owned());
            }
            actor.env.extend(env);
            actor.command = vec![std::env::var("CCCC_KILO_EXECUTABLE").unwrap_or_else(|_| "kilo".into()), "--pure".into()];
            store.mutate(&group.group_id, |doc| { doc.actors.push(actor.clone()); Ok(()) }).expect("actor");
            group = cccc_core::group_scope::attach(&store, &group.group_id,
                cccc_core::scope::detect(&project).expect("project scope")).expect("attach project");
            launch(&home, &group, &actor);
            let observed = std::panic::AssertUnwindSafe(async {
                let item = supervisor::lookup(&(group.group_id.clone(), actor.id.clone())).expect("owner");
                launch(&home, &group, &actor);
                assert_eq!(terminal_running(&group, &actor), !enabled);
                if !enabled {
                    assert!(tokio::task::block_in_place(|| cccc_runtime::wait_for_input_ready(
                        &group.group_id, &actor.id, Duration::from_secs(20), &AtomicBool::new(false))).expect("TUI ready"));
                }
                samples.push(sample(&item, "startup", enabled).await);
                if enabled {
                    exercise(&home, &group, &actor, &item, &requests.primary, &mut samples).await;
                    // Old connections must not close or cancel a replacement owner.
                    let stale = super::acquire(&group.group_id, &actor.id).expect("old lease").expect("lazy");
                    let old_generation = stale.interrupt_generation().expect("old generation");
                    tokio::task::block_in_place(|| supervisor::stop(&group.group_id, &actor.id)).expect("stop old");
                    launch(&home, &group, &actor);
                    let replacement = connect(&home, &group, &actor, false).await;
                    drop(stale);
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    assert!(terminal_running(&group, &actor), "stale lease closed replacement");
                    assert!(super::interrupt(&group.group_id, &actor.id, &old_generation).is_err(), "stale writer reached replacement");
                    drop(replacement);
                    wait_until(|| !terminal_running(&group, &actor)).await;
                }
            }).catch_unwind().await;
            tokio::task::block_in_place(|| supervisor::stop(&group.group_id, &actor.id)).expect("native cleanup");
            if let Err(panic) = observed { std::panic::resume_unwind(panic); }
        }
        eprintln!("LAZY_VIEWER_REQUEST_ROUTES {:?}", requests.routes.lock().expect("request observations"));
        assert_eq!(requests.unexpected.load(Ordering::Acquire), 0, "unclassified native model request");
        assert_eq!(requests.primary.load(Ordering::Acquire), 4, "duplicate or missing primary native model request");
        let report = json!({"runtime":"kilo","source":"native Linux process observations in CI",
            "rss_unit":"KiB","rss_kind":"sum of resident process trees, not PSS",
            "workload":"empty isolated checkout; same native CLI and local model configuration",
            "native_primary_model_requests":requests.primary.load(Ordering::Acquire),
            "native_auxiliary_model_requests":requests.auxiliary.load(Ordering::Acquire),
            "unexpected_model_requests":requests.unexpected.load(Ordering::Acquire),
            "request_routes":*requests.routes.lock().expect("request observations"), "samples":samples,
            "checks":{"queued_delivery":true,"ack_replay_once":true,"durable_receipt_retirement":true,
                "shared_viewer":true,"passive_cancel_refused":true,"writer_cancel":true,
                "last_close_preserves_turn":true,"stale_lease_and_writer_fenced":true}});
        eprintln!("LAZY_VIEWER_PROOF {report}");
        if let Some(path) = std::env::var_os("CCCC_LAZY_VIEWER_REPORT") {
            std::fs::write(path, serde_json::to_vec_pretty(&report).expect("report JSON")).expect("write observations");
        }
    }).catch_unwind().await;
    model.abort();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

fn launch(home: &HomeLayout, group: &GroupDoc, actor: &Actor) {
    tokio::task::block_in_place(|| {
        crate::ops::actor_runtime::apply(home, group, &actor.id, "actor.start")
    })
    .expect("native Actor start");
}

async fn exercise(
    home: &HomeLayout,
    group: &GroupDoc,
    actor: &Actor,
    item: &Arc<Session>,
    requests: &AtomicUsize,
    samples: &mut Vec<Value>,
) {
    let mut completions = item.managed.subscribe();
    let first = source(home, group, actor, "QUEUE_SLOW");
    assert_eq!(
        submit(home, group, actor, &first),
        BatchSubmission::Accepted
    );
    wait_until(|| requests.load(Ordering::Acquire) == 1).await;
    assert_eq!(
        submit(home, group, actor, &first),
        BatchSubmission::Accepted,
        "lost acknowledgement must reuse receipt"
    );
    assert_eq!(item.managed.delivery_receipt_count().await, 1);
    accepted(home, group, actor, &first);
    assert_eq!(item.managed.delivery_receipt_count().await, 0);
    // Real source, injected local saturation: no provider request was attempted.
    {
        let mut pending = item
            .lazy_viewer
            .as_ref()
            .expect("lazy")
            .pending
            .lock()
            .expect("pending");
        pending.extend(
            (0..256).map(|n| (format!("capacity-{n}"), BTreeSet::from([first.id.clone()]))),
        );
    }
    let never_sent = source(home, group, actor, "capacity control");
    let started = AtomicBool::new(false);
    assert_eq!(
        tokio::task::block_in_place(|| supervisor::submit_batch_marked(
            home,
            group,
            actor,
            std::slice::from_ref(&never_sent),
            &AtomicBool::new(false),
            &started
        )),
        BatchSubmission::Deferred
    );
    assert!(
        !started.load(Ordering::Acquire),
        "local capacity was marked submitted"
    );
    assert_eq!(requests.load(Ordering::Acquire), 1);
    item.lazy_viewer
        .as_ref()
        .expect("lazy")
        .pending
        .lock()
        .expect("pending")
        .clear();
    let second = source(home, group, actor, "Say hello.");
    let paired = source(home, group, actor, "Say hello again.");
    let batch = [second.clone(), paired.clone()];
    assert_eq!(
        submit_events(home, group, actor, &batch),
        BatchSubmission::Deferred
    );
    assert_eq!(
        crate::ops::runtime_delivery::latest_state(home, &group.group_id, &actor.id, &second.id)
            .expect("state")
            .expect("claim")
            .0,
        "claimed"
    );
    wait_completion(&mut completions, "completed").await;
    wait_idle(item).await;
    assert_eq!(
        requests.load(Ordering::Acquire),
        1,
        "first turn request count"
    );
    assert_eq!(
        submit_events(home, group, actor, &batch),
        BatchSubmission::Accepted
    );
    accepted(home, group, actor, &second);
    assert_eq!(
        item.managed.delivery_receipt_count().await,
        1,
        "partial durable acknowledgement retired the batch"
    );
    assert_eq!(
        submit_events(home, group, actor, &batch),
        BatchSubmission::Accepted
    );
    accepted(home, group, actor, &paired);
    assert_eq!(item.managed.delivery_receipt_count().await, 0);
    assert_eq!(
        submit_events(home, group, actor, &batch),
        BatchSubmission::Accepted,
        "fully durable acceptance was replayed after receipt retirement"
    );
    wait_until(|| requests.load(Ordering::Acquire) == 2).await;
    wait_completion(&mut completions, "completed").await;
    wait_idle(item).await;
    assert_eq!(
        requests.load(Ordering::Acquire),
        2,
        "paired turn request count"
    );
    assert!(!terminal_running(group, actor), "delivery opened a viewer");
    let mut writer = connect(home, group, actor, false).await;
    let viewer_pid = cccc_runtime::status(&group.group_id, &actor.id)
        .expect("viewer")
        .pid;
    let mut passive = connect(home, group, actor, true).await;
    assert_eq!(
        cccc_runtime::status(&group.group_id, &actor.id)
            .expect("shared viewer")
            .pid,
        viewer_pid
    );
    assert!(
        tokio::task::block_in_place(|| cccc_runtime::wait_for_input_ready(
            &group.group_id,
            &actor.id,
            Duration::from_secs(20),
            &AtomicBool::new(false)
        ))
        .expect("shared TUI ready")
    );
    samples.push(sample(item, "two_connections", true).await);
    let cancel = source(home, group, actor, "SLOW");
    assert_eq!(
        submit(home, group, actor, &cancel),
        BatchSubmission::Accepted
    );
    accepted(home, group, actor, &cancel);
    wait_until(|| requests.load(Ordering::Acquire) == 3).await;
    passive.write_all(b"\x03").await.expect("passive interrupt");
    let mut takeover = connect_mode(home, group, actor, false, true).await;
    writer
        .write_all(b"\x03")
        .await
        .expect("old writer interrupt");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        item.active_turn.lock().expect("turn").is_some(),
        "passive or replaced writer cancelled work"
    );
    takeover
        .write_all(b"\x03")
        .await
        .expect("current writer interrupt");
    wait_completion(&mut completions, "cancelled").await;
    wait_idle(item).await;
    assert_eq!(
        requests.load(Ordering::Acquire),
        3,
        "cancelled turn request count"
    );
    drop(writer);
    writer = takeover;
    let finishing = source(home, group, actor, "QUEUE_SLOW");
    assert_eq!(
        submit(home, group, actor, &finishing),
        BatchSubmission::Accepted
    );
    accepted(home, group, actor, &finishing);
    wait_until(|| requests.load(Ordering::Acquire) == 4).await;
    drop(writer);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        terminal_running(group, actor),
        "first close freed shared viewer"
    );
    drop(passive);
    wait_until(|| !terminal_running(group, actor)).await;
    assert!(item.running());
    assert!(
        item.active_turn.lock().expect("turn").is_some(),
        "last close stopped work"
    );
    wait_completion(&mut completions, "completed").await;
    wait_idle(item).await;
    assert_eq!(
        requests.load(Ordering::Acquire),
        4,
        "last-close turn request count"
    );
    samples.push(sample(item, "last_close", true).await);
    assert_eq!(item.managed.delivery_receipt_count().await, 0);
    let mut failed_close = super::acquire(&group.group_id, &actor.id)
        .expect("close control")
        .expect("lease");
    let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = item.stop_lock.lock().expect("stop lock");
        panic!("injected viewer teardown failure");
    }));
    assert!(poisoned.is_err());
    let owner = std::mem::take(&mut failed_close.0);
    tokio::task::block_in_place(|| super::release(owner));
    drop(failed_close);
    assert_eq!(
        item.lazy_viewer
            .as_ref()
            .expect("lazy")
            .clients
            .load(Ordering::Acquire),
        0
    );
    assert!(
        terminal_running(group, actor),
        "failed close lost its viewer"
    );
    assert!(item.running(), "failed close stopped the provider");
    item.stop_lock.clear_poison();
    let retry_close = super::acquire(&group.group_id, &actor.id)
        .expect("retry lease")
        .expect("lazy");
    drop(retry_close);
    wait_until(|| !terminal_running(group, actor)).await;
    assert!(item.running());
}

fn source(home: &HomeLayout, group: &GroupDoc, actor: &Actor, text: &str) -> Event {
    let mut event = Event::new("chat.message", &group.group_id);
    event.by = "user".into();
    event.data = json!({"text":text,"to":[actor.id]})
        .as_object()
        .expect("message")
        .clone();
    let store = GroupStore::new(home.clone()).expect("store");
    ledger::append(&store.ledger_path(&group.group_id).expect("ledger"), &event).expect("source");
    crate::ops::runtime_delivery::append_state(
        home,
        &group.group_id,
        &actor.id,
        &actor.created_at,
        &event.id,
        "managed_session",
        crate::ops::runtime_delivery::DeliveryOutcome::Claimed,
    )
    .expect("durable claim");
    event
}

fn accepted(home: &HomeLayout, group: &GroupDoc, actor: &Actor, event: &Event) {
    crate::ops::runtime_delivery::append_state(
        home,
        &group.group_id,
        &actor.id,
        &actor.created_at,
        &event.id,
        "managed_session",
        crate::ops::runtime_delivery::DeliveryOutcome::Accepted,
    )
    .expect("durable acceptance");
}

fn submit(home: &HomeLayout, group: &GroupDoc, actor: &Actor, event: &Event) -> BatchSubmission {
    submit_events(home, group, actor, std::slice::from_ref(event))
}

fn submit_events(
    home: &HomeLayout,
    group: &GroupDoc,
    actor: &Actor,
    events: &[Event],
) -> BatchSubmission {
    tokio::task::block_in_place(|| {
        supervisor::submit_batch(home, group, actor, events, &AtomicBool::new(false))
    })
}

async fn connect(
    home: &HomeLayout,
    group: &GroupDoc,
    actor: &Actor,
    passive: bool,
) -> DuplexStream {
    connect_mode(home, group, actor, passive, false).await
}

async fn connect_mode(
    home: &HomeLayout,
    group: &GroupDoc,
    actor: &Actor,
    passive: bool,
    takeover: bool,
) -> DuplexStream {
    let (client, server) = tokio::io::duplex(16 * 1024);
    let request = DaemonRequest {
        v: 1,
        op: "term_attach".into(),
        args: json!({"group_id":group.group_id,
        "actor_id":actor.id,"mode":if passive {"viewer"} else {"control"},"takeover":takeover})
        .as_object()
        .expect("args")
        .clone(),
    };
    let home = home.clone();
    let (shutdown, receiver) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        let _shutdown = shutdown;
        let _ =
            crate::server_terminal_attach::handle(BufReader::new(server), home, request, receiver)
                .await;
    });
    // Keep a sender alive in the server task: a dropped watch sender is a shutdown.
    let mut client = BufReader::new(client);
    let mut header = String::new();
    tokio::time::timeout(Duration::from_secs(30), client.read_line(&mut header))
        .await
        .expect("attach deadline")
        .expect("attach header");
    let response: Value = serde_json::from_str(&header).expect("attach response");
    assert_eq!(response["ok"], true, "terminal attachment failed");
    assert_eq!(response["result"]["terminal_writable"], !passive);
    client.into_inner()
}

fn terminal_running(group: &GroupDoc, actor: &Actor) -> bool {
    cccc_runtime::status(&group.group_id, &actor.id).is_ok_and(|status| status.running)
}

async fn wait_idle(item: &Session) {
    wait_until(|| {
        item.active_turn.lock().is_ok_and(|turn| turn.is_none())
            && item.status.lock().is_ok_and(|state| state.status == "idle")
    })
    .await;
}

async fn wait_completion(
    events: &mut tokio::sync::broadcast::Receiver<crate::ops::codex_voice_analyst::AnalystEvent>,
    expected: &str,
) {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let event = events.recv().await.expect("native protocol event");
            if event.message["method"] == "turn/completed" {
                assert_eq!(event.message["params"]["turn"]["status"], expected);
                break;
            }
        }
    })
    .await
    .expect("native completion deadline");
}

async fn wait_until(condition: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("native observation deadline");
}

async fn sample(item: &Session, phase: &str, enabled: bool) -> Value {
    let mut observations = Vec::new();
    for _ in 0..5 {
        let mut seen = BTreeSet::new();
        let mut total = tree_rss(item.managed.process_id().expect("provider pid"), &mut seen);
        if let Ok(viewer) = cccc_runtime::status(&item.group_id, &item.actor_id)
            && viewer.running
            && let Some(pid) = viewer.pid
        {
            total += tree_rss(pid, &mut seen);
        }
        observations.push(total);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(observations.iter().all(|rss| *rss > 0));
    let mut sorted = observations.clone();
    sorted.sort_unstable();
    json!({"phase":phase,"headless":enabled,"rss_kib":observations,"median_rss_kib":sorted[2]})
}

fn tree_rss(pid: u32, seen: &mut BTreeSet<u32>) -> u64 {
    if !seen.insert(pid) {
        return 0;
    }
    let status =
        std::fs::read_to_string(format!("/proc/{pid}/status")).expect("live process status");
    let rss = status
        .lines()
        .find_map(|line| {
            line.strip_prefix("VmRSS:")
                .and_then(|line| line.split_whitespace().next())
                .and_then(|value| value.parse::<u64>().ok())
        })
        .expect("resident bytes");
    let children =
        std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")).expect("child list");
    rss + children
        .split_whitespace()
        .map(|child| tree_rss(child.parse().expect("child pid"), seen))
        .sum::<u64>()
}
