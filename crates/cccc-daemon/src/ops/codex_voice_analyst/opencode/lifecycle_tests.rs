//! Regression: a single large lifecycle event must not kill the actor session.
//!
//! This exercises the real `attach()` subscription against a scratch HTTP
//! server, with no OpenCode installation, no model call, and no live daemon
//! home. The event carries one tool attachment whose base64 payload is larger
//! than the previous 512 KiB cap; before the cap was raised the subscription
//! task returned InvalidData and the session was torn down instead of
//! settling the turn.
#![cfg(unix)]

use super::super::MANAGED_AGENT_DISCONNECTED_METHOD;
use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn event_larger_than_the_previous_cap_still_settles_the_turn() {
    let temp = tempfile::tempdir().expect("isolated fixture");
    let script = r#"i=0
while IFS= read -r line; do
  i=$((i+1))
  printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"session-owned"}}\n' "$i"
done
"#;
    let (owner, stdin, stdout) = process::spawn_piped(
        &["/bin/sh".into(), "-c".into(), script.into()],
        temp.path(),
        &BTreeMap::new(),
        "sse-fixture",
    )
    .expect("spawn fixture ACP");
    let protocol = AcpClient::new(
        stdin,
        stdout,
        "generation-sse".into(),
        "opencode",
        PermissionPolicy::Reject,
        PromptCompletion::SessionEvents,
    )
    .expect("ACP bridge");
    protocol
        .request("initialize", json!({}), Duration::from_secs(5))
        .await
        .expect("initialize");
    protocol
        .request("session/new", json!({}), Duration::from_secs(5))
        .await
        .expect("owned session");
    let user_text = "prompt-".repeat(8);
    let image_url = format!("data:image/png;base64,{}", "A".repeat(600_000));
    protocol
        .register_native_input("narrow-input", &user_text)
        .await
        .expect("register exact input");
    let mut events = protocol.subscribe();
    // One event over the old 512 KiB cap, then an idle that settles the turn.
    // If the oversized event tears the subscription down, the turn never
    // completes and the timeout below fails.
    let payloads = [
        json!({"type":"message.updated","properties":{"info":{"id":"user-1","sessionID":"session-owned","role":"user"}}}),
        json!({"type":"message.part.updated","properties":{"part":{"id":"user-part","sessionID":"session-owned","messageID":"user-1","type":"text","text":user_text}}}),
        json!({"type":"message.updated","properties":{"info":{"id":"assistant-1","sessionID":"session-owned","role":"assistant","parentID":"user-1"}}}),
        json!({"type":"session.status","properties":{"sessionID":"session-owned","status":{"type":"busy"}}}),
        json!({"type":"message.part.updated","properties":{"part":{"id":"tool-part","sessionID":"session-owned","messageID":"assistant-1","type":"tool","callID":"tool-1","tool":"read","state":{"status":"completed","input":{},"output":"image","metadata":{},"attachments":[{"type":"file","mime":"image/png","url":image_url}]}}}}),
        json!({"type":"session.status","properties":{"sessionID":"session-owned","status":{"type":"idle"}}}),
    ];
    let mut wire = String::new();
    for payload in &payloads {
        let line = format!("data: {payload}\r\n\r\n");
        if payload["properties"]["part"]["callID"] == "tool-1" {
            assert!(
                line.len() > 512 * 1024,
                "fixture must exceed the previous cap, got {} bytes",
                line.len()
            );
            assert!(
                line.len() < 16 * 1024 * 1024,
                "fixture must stay within the raised cap"
            );
        }
        wire.push_str(&line);
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("scratch server");
    let endpoint = format!(
        "http://{}",
        listener.local_addr().expect("listener address")
    );
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("SSE connection");
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            request.push(socket.read_u8().await.expect("request headers"));
        }
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await.expect("SSE headers");
        socket
            .write_all(format!("{:x}\r\n", wire.len()).as_bytes())
            .await
            .expect("chunk framing");
        for fragment in wire.as_bytes().chunks(64 * 1024) {
            if socket.write_all(fragment).await.is_err() {
                return;
            }
        }
        socket.write_all(b"\r\n").await.expect("chunk ending");
        let _ = stopped.await; // keep the stream alive through the assertions
    });
    lifecycle::attach(
        &protocol,
        &endpoint,
        "fixture",
        "private",
        "session-owned",
        temp.path(),
    )
    .await
    .expect("attach subscription");
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = events.recv().await.expect("lifecycle event");
            assert_ne!(
                event.message["method"], MANAGED_AGENT_DISCONNECTED_METHOD,
                "a large event must not disconnect the session: {}",
                event.message
            );
            if event.message["method"].as_str() == Some("turn/completed") {
                assert_eq!(event.message["params"]["turn"]["status"], "completed");
                break;
            }
        }
    })
    .await
    .expect("the oversized event must not prevent the turn from settling");
    protocol
        .request("fixture/ping", json!({}), Duration::from_secs(5))
        .await
        .expect("session remains usable");
    protocol.close().await;
    owner.stop().expect("stop scratch ACP");
    let _ = stop.send(());
    server.await.expect("stop scratch SSE server");
}