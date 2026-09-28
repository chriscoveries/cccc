use super::super::*;
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Serve a resume-only session and capture every `thread/resume` request.
/// The first `rejections` resumes are answered with an invalid-params error, as
/// an app-server that does not know the `excludeTurns` field would; later ones
/// get a metadata-only reply with no turns at all.
async fn resume_probe_server(
    rejections: usize,
) -> (String, tokio::task::JoinHandle<()>, Arc<Mutex<Vec<Value>>>) {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener");
    let endpoint = format!("ws://{}", listener.local_addr().expect("address"));
    let captured = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&captured);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let mut socket = tokio_tungstenite::accept_async(stream)
            .await
            .expect("websocket");
        let mut resumes = 0_usize;
        while let Some(frame) = socket.next().await {
            let frame = match frame {
                Ok(frame) => frame,
                Err(_) => break,
            };
            let Message::Text(text) = frame else {
                continue;
            };
            let request: Value = serde_json::from_str(&text).expect("request");
            let id = request["id"].as_u64().expect("request id");
            let response = match request["method"].as_str().expect("method") {
                "initialize" => json!({"id": id, "result": {}}),
                "thread/resume" => {
                    resumes += 1;
                    sink.lock()
                        .expect("capture")
                        .push(request["params"].clone());
                    if resumes <= rejections {
                        json!({"id": id, "error": {"code": -32602, "message":
                            "invalid params: unknown field `excludeTurns`, expected one of `threadId`, `cwd`"}})
                    } else {
                        // Metadata only: the client must take the thread id from a
                        // reply that carries no turns.
                        json!({"id": id, "result": {"thread": {"id": "thread-1"}}})
                    }
                }
                "thread/start" => panic!("resume must not fall back to a fresh thread"),
                method => panic!("unexpected method: {method}"),
            };
            socket
                .send(Message::Text(response.to_string().into()))
                .await
                .expect("response");
        }
    });
    (endpoint, server, captured)
}

async fn connect_resuming(endpoint: String, purpose: SessionPurpose) -> AnalystSession {
    AnalystSession::connect(ConnectConfig {
        binding: WorkspaceBinding {
            root: std::env::current_dir().expect("cwd"),
        },
        generation: "generation-resume-metadata".into(),
        endpoint,
        remote_tui_prefix: vec![PathBuf::from("codex").to_string_lossy().into_owned()],
        environment: Default::default(),
        resume_thread_id: Some("thread-1".into()),
        process: None,
        delegations: HashMap::new(),
        purpose,
    })
    .await
    .expect("connect")
}

#[tokio::test]
async fn codex_resume_takes_the_thread_id_from_a_metadata_only_reply() {
    let (endpoint, server, captured) = resume_probe_server(0).await;
    let session = connect_resuming(endpoint, SessionPurpose::VoiceAnalyst).await;

    assert_eq!(session.thread_id(), "thread-1");
    assert!(
        session.thread_resumed,
        "the same thread must be reported as resumed"
    );
    let resumes = captured.lock().expect("capture").clone();
    assert_eq!(resumes.len(), 1, "one resume, no retry");
    assert_eq!(resumes[0]["threadId"], "thread-1");
    assert_eq!(
        resumes[0]["excludeTurns"], true,
        "the resume must ask for a metadata-only reply"
    );
    assert!(
        resumes[0].get("historyMode").is_none(),
        "resume must not request legacy history"
    );
    drop(session);
    server.await.expect("fake server");
}

#[tokio::test]
async fn an_app_server_that_rejects_exclude_turns_still_resumes_the_same_thread() {
    let (endpoint, server, captured) = resume_probe_server(1).await;
    // Actor purpose: this is the path that silently starts a fresh thread when a
    // resume fails, so a retry that did not happen would show up as a
    // `thread/start` request (the server panics on one).
    let session = connect_resuming(endpoint, SessionPurpose::Actor).await;

    assert_eq!(session.thread_id(), "thread-1");
    assert!(
        session.thread_resumed,
        "an unknown-field rejection must not cost the actor its thread"
    );
    let resumes = captured.lock().expect("capture").clone();
    assert_eq!(resumes.len(), 2, "exactly one retry");
    assert_eq!(resumes[0]["excludeTurns"], true);
    assert!(
        resumes[1].get("excludeTurns").is_none(),
        "the retry must drop the field the server rejected"
    );
    assert_eq!(resumes[1]["threadId"], "thread-1");
    drop(session);
    server.await.expect("fake server");
}
