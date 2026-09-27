use super::*;

/// Regression: the app-server answers `thread/resume` with a single
/// unfragmented message that grows with thread history (~30 MiB observed on
/// a long-lived actor). The previous connect used tungstenite's defaults
/// (16 MiB frame / 64 MiB message), which rejected it with Error::Capacity on
/// every reconnect. A message above the old frame cap must now be delivered.
#[tokio::test]
async fn connect_accepts_messages_larger_than_the_old_default_cap() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind probe server");
    let address = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let mut config =
            tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default();
        config.max_frame_size = None;
        config.max_message_size = None;
        let mut server = tokio_tungstenite::accept_async_with_config(
            stream,
            Some(config),
        )
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
