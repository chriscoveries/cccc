use super::*;

/// `thread/resume` returns the whole thread history as one unfragmented message (about 30 MiB
/// on a long-lived thread). With tungstenite's default 16 MiB frame cap the client rejected it
/// with `Capacity` on every reconnect. A message above the old cap must now be delivered.
#[tokio::test]
async fn connect_accepts_messages_larger_than_the_old_default_cap() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind probe server");
    let address = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let mut config = WebSocketConfig::default();
        config.max_frame_size = None;
        config.max_message_size = None;
        let mut server = tokio_tungstenite::accept_async_with_config(stream, Some(config))
            .await
            .expect("accept websocket");
        server
            .send(Message::Text("x".repeat(20 << 20).into()))
            .await
            .expect("send oversized message");
        tokio::time::sleep(Duration::from_secs(2)).await;
    });

    let mut socket = connect_with_retry(&format!("ws://{address}"))
        .await
        .expect("connect");
    let message = tokio::time::timeout(Duration::from_secs(10), socket.next())
        .await
        .expect("read deadline")
        .expect("stream ended")
        .expect("oversized message must be delivered, not rejected as Capacity");
    assert_eq!(message.into_data().len(), 20 << 20);
}
