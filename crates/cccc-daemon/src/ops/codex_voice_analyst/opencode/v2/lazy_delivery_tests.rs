//! Delivery controls using the existing V2 loopback protocol harness.
use super::*;

#[tokio::test]
async fn v2_lazy_delivery_uses_the_http_queue_and_retains_receipts_until_settlement() {
    use crate::ops::codex_voice_analyst::{AnalystSession, WorkspaceBinding};
    use axum::{Json, Router, routing::post};
    use std::future::IntoFuture;
    use std::sync::atomic::AtomicUsize;
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("backend");
    let (mut inner, _) = inner();
    inner.endpoint = format!("http://{}", listener.local_addr().expect("address"));
    let server = tokio::spawn(axum::serve(listener, Router::new().route("/api/session/ses_fixture/prompt", post(move |Json(body): Json<Value>| {
        let observed = Arc::clone(&observed);
        async move {
            assert_eq!(body["delivery"], "queue");
            observed.fetch_add(1, Ordering::AcqRel);
            Json(json!({"data":{"id":"msg_delivery","sessionID":"ses_fixture","delivery":"queue"}}))
        }
    }))).into_future());
    let (stop, _) = watch::channel(false);
    let session = AnalystSession {
        binding: WorkspaceBinding {
            root: std::env::current_dir().expect("cwd"),
        },
        generation: "generation".into(),
        runtime: cccc_contracts::ActorRuntime::Opencode,
        endpoint: String::new(),
        thread_id: "ses_fixture".into(),
        remote_tui_prefix: Vec::new(),
        environment: BTreeMap::new(),
        protocol: ManagedProtocol::OpenCode(OpenCodeClient {
            inner: Arc::new(inner),
            stop,
            lease: None,
        }),
        process: None,
        auxiliary_processes: Vec::new(),
        native_tui_command: None,
        cleanup_paths: Vec::new(),
        thread_resumed: false,
        delegations: Mutex::new(std::collections::HashMap::new()),
    };
    let started = AtomicBool::new(false);
    let receipt = session
        .start_delivery("delivery", "queued input", &started)
        .await
        .expect("admitted");
    assert!(started.load(Ordering::Acquire));
    assert_eq!(receipt.turn_id, "msg_delivery");
    assert_eq!(
        session
            .start_delivery("delivery", "queued input", &AtomicBool::new(false))
            .await
            .expect("same receipt")
            .turn_id,
        receipt.turn_id
    );
    assert_eq!(calls.load(Ordering::Acquire), 1);
    let local = AtomicBool::new(false);
    assert_eq!(
        session
            .start_delivery("later", "later input", &local)
            .await
            .expect_err("existing controlled input")
            .kind(),
        io::ErrorKind::WouldBlock
    );
    assert!(!local.load(Ordering::Acquire));
    assert_eq!(calls.load(Ordering::Acquire), 1);
    assert!(
        session
            .try_interrupt(session.generation(), &receipt.turn_id)
            .is_err(),
        "V2 must not use session-scoped ACP cancellation"
    );
    session.forget_delivery("delivery").await;
    assert!(session.delegations.lock().await.is_empty());
    server.abort();
}

fn inner() -> (Inner, broadcast::Receiver<AnalystEvent>) {
    let (events, receiver) = broadcast::channel(128);
    (
        Inner {
            http: reqwest::Client::new(),
            endpoint: "http://127.0.0.1:1".into(),
            session_id: "ses_fixture".into(),
            generation: "generation".into(),
            events,
            state: Mutex::new(events::State::default()),
            running: AtomicBool::new(true),
        },
        receiver,
    )
}
