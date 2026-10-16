//! Real socket streaming at the existing pre-input admission API.
use super::*;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncBufReadExt, BufReader};

const NONCE_MARK: &[u8] = b"\x1b_cc-d-imark;{\"kind\":\"prompt_idle\",\"nonce\":\"0123456789abcdef0123456789abcdef\"}\x1b\\";
const PROMPT: &str = "❯\r\n⏵⏵ bypass permissions on (shift+tab to cycle)";
fn paint(order: &str, text: &str) -> Vec<u8> {
    format!("\x1b[?2026h{order}{text}\x1b[?2026l").into_bytes()
}

async fn exchange(chunks: Vec<Vec<u8>>) -> (Option<io::Error>, Vec<u8>, bool) {
    let temp = tempfile::tempdir().expect("private streamed readiness fixture");
    let key = temp.path().join("key");
    std::fs::write(&key, "0123456789abcdef0123456789abcdef").expect("key");
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).expect("key mode");
    let address = temp.path().join("control.sock");
    let endpoint = Endpoint::fixture(address.to_string_lossy().into_owned(), key);
    let listener = tokio::net::UnixListener::bind(address).expect("private control socket");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("attachment");
        let mut attachment = BufReader::new(stream);
        let mut request = String::new();
        attachment
            .read_line(&mut request)
            .await
            .expect("attach request");
        assert_eq!(
            serde_json::from_str::<Value>(&request).expect("JSON")["op"],
            "attach"
        );
        attachment.get_mut().write_all(b"{\"ok\":true,\"op\":\"attach\",\"imarkNonce\":\"0123456789abcdef0123456789abcdef\"}\n").await.expect("ack");
        let probes = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.expect("has connection");
                let mut stream = BufReader::new(stream);
                let mut request = String::new();
                stream.read_line(&mut request).await.expect("has request");
                assert_eq!(
                    serde_json::from_str::<Value>(&request).expect("JSON")["op"],
                    "has"
                );
                stream
                    .get_mut()
                    .write_all(b"{\"ok\":true,\"op\":\"has\",\"alive\":true,\"ready\":true}\n")
                    .await
                    .expect("ready");
            }
        });
        attachment
            .get_mut()
            .write_all(NONCE_MARK)
            .await
            .expect("real nonce-bound mark");
        for chunk in chunks {
            attachment
                .get_mut()
                .write_all(&chunk)
                .await
                .expect("streamed native bytes");
            attachment.get_mut().flush().await.expect("stream flush");
            tokio::task::yield_now().await;
        }
        let mut observed = Vec::new();
        let read = tokio::time::timeout(
            Duration::from_secs(15),
            attachment.take(128).read_to_end(&mut observed),
        )
        .await;
        probes.abort();
        read.expect("client must return and close attachment")
            .expect("native input read");
        observed
    });
    let verified = Arc::new(AtomicBool::new(false));
    let verification = Arc::clone(&verified);
    let result = model_input(&endpoint, "01234567", "opus", move || {
        verification.store(true, Ordering::Release);
        Ok(())
    })
    .await;
    let error = match result {
        Ok(stream) => {
            drop(stream);
            None
        }
        Err(error) => Some(error),
    };
    let observed = server.await.expect("streaming server");
    (error, observed, verified.load(Ordering::Acquire))
}

#[tokio::test]
async fn actual_streamed_reordered_repaints_admit_exactly_one_model_command() {
    for order in [
        "\x1b[2J\x1b[3J\x1b[H",
        "\x1b[H\x1b[2J",
        "\x1b[3J\x1b[2J\x1b[H",
    ] {
        let mut chunks = vec![vec![b'x'; 384 * 1024]];
        chunks.extend(paint(order, PROMPT).into_iter().map(|byte| vec![byte]));
        let (error, observed, verified) = exchange(chunks).await;
        assert!(error.is_none(), "native order {order:?}: {error:?}");
        assert!(verified);
        assert_eq!(observed, b"/model opus\r");
    }
}

#[tokio::test]
async fn evicted_stream_without_readable_repaint_requests_fresh_before_any_model_input() {
    let (error, observed, verified) =
        exchange(vec![vec![b'x'; 384 * 1024], paint("", PROMPT)]).await;
    assert!(
        observed.is_empty(),
        "unknown native paint never receives input"
    );
    assert!(!verified);
    let error = error.expect("no readable reconstruction arrived");
    assert!(
        super::super::model_switch::is_requires_fresh(&error),
        "pre-input compatibility outcome: {error}"
    );
}

#[tokio::test]
async fn readable_draft_after_eviction_remains_strict_and_receives_no_input() {
    let (error, observed, verified) = exchange(vec![
        vec![b'x'; 384 * 1024],
        paint(
            "\x1b[2J\x1b[H",
            "❯ existing draft\r\n⏵⏵ bypass permissions on",
        ),
    ])
    .await;
    assert!(observed.is_empty());
    assert!(!verified);
    assert!(!super::super::model_switch::is_requires_fresh(
        &error.expect("readable draft is not consent")
    ));
}

#[tokio::test]
async fn streamed_osc_and_dcs_paints_never_authorize_or_mix_two_unrelated_frames() {
    let mut fake = b"\x1b]title;".to_vec();
    fake.extend(paint("\x1b[H\x1b[2J", PROMPT));
    fake.extend(b"\x1b\\\x1bPignored;");
    fake.extend(paint("\x1b[2J\x1b[3J\x1b[H", PROMPT));
    fake.extend(b"\x1b\\");
    fake.extend(paint("\x1b[2J", "unrelated frame"));
    fake.extend(paint("\x1b[H", PROMPT));
    let mut chunks = vec![vec![b'x'; 384 * 1024]];
    chunks.extend(fake.chunks(3).map(<[u8]>::to_vec));
    let (error, observed, verified) = exchange(chunks).await;
    assert!(error.is_some());
    assert!(observed.is_empty());
    assert!(!verified);
}
