//! T844: a Codex thread must open on the model the actor declares.
//!
//! The app-server starts a thread on the host's `config.toml` default unless
//! the request carries a model, and `thread/resume` ignores a model for a
//! thread that already exists. Both paths are pinned here with a fake
//! app-server, so a regression shows up as a request assertion rather than as
//! an actor quietly running as the wrong model for days.

use super::super::*;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

const DECLARED: &str = "cccc-test-model";

/// Serve one connect() and return every method the client asked for, in order.
async fn serve(
    script: Vec<(&'static str, fn(&Value) -> Value)>,
) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener");
    let endpoint = format!("ws://{}", listener.local_addr().expect("address"));
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let mut socket = tokio_tungstenite::accept_async(stream)
            .await
            .expect("websocket");
        let mut seen = Vec::new();
        let mut index = 0usize;
        while let Some(Ok(Message::Text(text))) = socket.next().await {
            let request: Value = serde_json::from_str(&text).expect("JSON");
            let method = request["method"].as_str().expect("method").to_owned();
            seen.push(method.clone());
            let (expected, reply) = *script
                .get(index)
                .unwrap_or_else(|| panic!("unexpected extra request: {method}"));
            assert_eq!(method, expected, "request {index}");
            let result = reply(&request);
            index += 1;
            socket
                .send(Message::Text(
                    json!({"id":request["id"],"result":result})
                        .to_string()
                        .into(),
                ))
                .await
                .expect("reply");
            if index == script.len() {
                break;
            }
        }
        let _ = tokio::time::timeout(Duration::from_secs(1), socket.next()).await;
        seen
    });
    (endpoint, server)
}

fn ok(_: &Value) -> Value {
    json!({})
}

/// A thread that opened on the declared model.
fn thread(model: &str) -> Value {
    json!({"thread":{"id":"thread-model","model":model}})
}

fn start_carries_the_model(request: &Value) -> Value {
    assert_eq!(
        request["params"]["model"], DECLARED,
        "thread/start must carry the actor's model, or the thread runs as the host default"
    );
    thread(DECLARED)
}

fn analyst_config(endpoint: String, resume: Option<&str>) -> ConnectConfig {
    ConnectConfig {
        binding: WorkspaceBinding {
            root: std::env::current_dir().expect("cwd"),
        },
        generation: "t844".into(),
        endpoint,
        remote_tui_prefix: vec!["codex".into()],
        environment: Default::default(),
        resume_thread_id: resume.map(str::to_owned),
        process: None,
        delegations: HashMap::new(),
        purpose: SessionPurpose::Actor,
        model: DECLARED.into(),
    }
}

#[tokio::test]
async fn a_new_thread_is_started_on_the_actors_model() {
    let (endpoint, server) = serve(vec![
        ("initialize", ok),
        ("thread/start", start_carries_the_model),
        ("thread/name/set", ok),
        ("thread/read", |_| json!({"thread":{"id":"thread-model","turns":[]}})),
    ])
    .await;
    AnalystSession::connect(analyst_config(endpoint, None))
        .await
        .expect("launch");
    let seen = server.await.expect("server");
    assert_eq!(seen[1], "thread/start");
}

#[tokio::test]
async fn a_resumed_thread_is_moved_to_the_actors_model() {
    // The resume response reports the model the rollout recorded - the host
    // default in the failure this card fixes - and resume ignores a model, so
    // the session must ask for the declared one and read the thread back.
    let (endpoint, server) = serve(vec![
        ("initialize", ok),
        ("thread/resume", |_| thread("host-default")),
        ("thread/settings/update", |request| {
            assert_eq!(request["params"]["model"], DECLARED);
            assert_eq!(request["params"]["threadId"], "thread-model");
            json!({})
        }),
        ("thread/read", |request| {
            assert_eq!(request["params"]["includeTurns"], false);
            thread(DECLARED)
        }),
    ])
    .await;
    AnalystSession::connect(analyst_config(endpoint, Some("thread-model")))
        .await
        .expect("launch");
    let seen = server.await.expect("server");
    assert!(
        seen.contains(&"thread/settings/update".to_owned()),
        "a resumed thread on the wrong model was left alone: {seen:?}"
    );
}

#[tokio::test]
async fn a_thread_that_will_not_adopt_the_model_fails_the_launch() {
    let (endpoint, _server) = serve(vec![
        ("initialize", ok),
        ("thread/resume", |_| thread("host-default")),
        ("thread/settings/update", ok),
        ("thread/read", |_| thread("host-default")),
    ])
    .await;
    let error = AnalystSession::connect(analyst_config(endpoint, Some("thread-model")))
        .await
        .err()
        .expect("a thread on the wrong model must not be reported as running");
    assert!(
        error.to_string().contains(DECLARED),
        "failure must name the declared model: {error}"
    );
}
