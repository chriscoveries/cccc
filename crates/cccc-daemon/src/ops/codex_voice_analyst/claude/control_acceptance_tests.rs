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
    assert!(
        verified,
        "fresh compatibility must revalidate native identity"
    );
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
    assert!(
        verified,
        "fresh compatibility must revalidate native identity"
    );
    assert!(super::super::model_switch::is_requires_fresh(
        &error.expect("unreconstructable frames remain pre-input compatibility")
    ));
}

#[tokio::test]
async fn known_draft_followed_by_partial_paint_does_not_become_fresh_compatibility() {
    let (error, observed, verified) = exchange(vec![
        vec![b'x'; 384 * 1024],
        paint(
            "\x1b[2J\x1b[H",
            "❯ existing draft\r\n⏵⏵ bypass permissions on",
        ),
        b"\x1b[?2026h\x1b[H".to_vec(),
    ])
    .await;
    assert!(observed.is_empty());
    assert!(!verified);
    assert!(!super::super::model_switch::is_requires_fresh(
        &error.expect("known draft remains strict through incomplete next paint")
    ));
}

#[derive(Clone, Copy, Debug)]
struct ConsumedFeed {
    bytes: usize,
    deadline: tokio::time::Instant,
}

struct FeedObservation {
    token: u64,
    bytes: usize,
    sender: Option<tokio::sync::oneshot::Sender<ConsumedFeed>>,
}

fn feed_observations()
-> &'static std::sync::Mutex<std::collections::HashMap<String, FeedObservation>> {
    static OBSERVATIONS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, FeedObservation>>,
    > = std::sync::OnceLock::new();
    OBSERVATIONS.get_or_init(Default::default)
}

struct FeedScope {
    address: String,
    token: u64,
}

impl Drop for FeedScope {
    fn drop(&mut self) {
        let mut observations = feed_observations().lock().expect("feed registry");
        if observations
            .get(&self.address)
            .is_some_and(|observation| observation.token == self.token)
        {
            observations.remove(&self.address);
        }
    }
}

fn observe_consumed_feed(
    endpoint: &Endpoint,
) -> (FeedScope, tokio::sync::oneshot::Receiver<ConsumedFeed>) {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let token = NEXT.fetch_add(1, Ordering::Relaxed);
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let previous = feed_observations().lock().expect("feed registry").insert(
        endpoint.address.clone(),
        FeedObservation {
            token,
            bytes: 0,
            sender: Some(sender),
        },
    );
    assert!(
        previous.is_none(),
        "fixture endpoint has one observation owner"
    );
    (
        FeedScope {
            address: endpoint.address.clone(),
            token,
        },
        receiver,
    )
}

// Observation only: called after the real Screen::feed, never from the fake
// server's write completion. The one-shot cannot fire before >128 KiB have
// actually reached the bounded scanner, and only the exact endpoint can fire it.
pub(super) fn after_screen_feed(endpoint: &Endpoint, count: usize, deadline: tokio::time::Instant) {
    let mut observations = feed_observations().lock().expect("feed registry");
    let Some(observation) = observations.get_mut(&endpoint.address) else {
        return;
    };
    observation.bytes += count;
    if observation.bytes >= 192 * 1024
        && let Some(sender) = observation.sender.take()
    {
        let _ = sender.send(ConsumedFeed {
            bytes: observation.bytes,
            deadline,
        });
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReadinessBlocker {
    StalledHas,
    Busy,
    Dead,
    WrongMark,
    MissingMark,
    IdentityChanged,
}

struct ProbeReport {
    consumed: Option<ConsumedFeed>,
    stalled_has: bool,
    blocked_replies: usize,
}

struct BlockedExchange {
    error: Option<io::Error>,
    observed: Vec<u8>,
    verified: bool,
    client_completed: bool,
    report: ProbeReport,
}

async fn blocked_evicted_exchange(blocker: ReadinessBlocker) -> BlockedExchange {
    let temp = tempfile::tempdir().expect("private negative readiness fixture");
    let key = temp.path().join("key");
    std::fs::write(&key, "0123456789abcdef0123456789abcdef").expect("key");
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).expect("key mode");
    let address = temp.path().join("control.sock");
    let endpoint = Endpoint::fixture(address.to_string_lossy().into_owned(), key);
    let listener = tokio::net::UnixListener::bind(address).expect("negative control socket");
    let (_scope, mut consumed_receiver) = observe_consumed_feed(&endpoint);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("attachment");
        let mut attachment = BufReader::new(stream);
        let mut line = String::new();
        attachment
            .read_line(&mut line)
            .await
            .expect("attach request");
        assert_eq!(
            serde_json::from_str::<Value>(&line).expect("JSON")["op"],
            "attach"
        );
        attachment.get_mut().write_all(b"{\"ok\":true,\"op\":\"attach\",\"imarkNonce\":\"0123456789abcdef0123456789abcdef\"}\n").await.expect("ack");
        let (stop_sender, mut stop_receiver) = tokio::sync::oneshot::channel::<()>();
        let probes = tokio::spawn(async move {
            let mut report = ProbeReport {
                consumed: None,
                stalled_has: false,
                blocked_replies: 0,
            };
            loop {
                let accepted = tokio::select! {
                    _ = &mut stop_receiver => break,
                    accepted = listener.accept() => accepted,
                };
                let (stream, _) = accepted.expect("has connection");
                let mut stream = BufReader::new(stream);
                let mut line = String::new();
                let read = tokio::select! {
                    _ = &mut stop_receiver => break,
                    read = stream.read_line(&mut line) => read,
                };
                // Cancellation may abandon a connected probe before its frame
                // completes. Do not strand the fixture's join on that peer.
                if read.unwrap_or(0) == 0 || !line.ends_with('\n') {
                    continue;
                }
                assert_eq!(
                    serde_json::from_str::<Value>(&line).expect("JSON")["op"],
                    "has"
                );
                if report.consumed.is_none() {
                    report.consumed = consumed_receiver.try_recv().ok();
                }
                // Healthy probes continue until the actual model_input deadline
                // is less than REQUEST_TIMEOUT away. Only then stall a real HAS
                // RPC, with consumed eviction already independently observed.
                if blocker == ReadinessBlocker::StalledHas
                    && let Some(consumed) = report.consumed
                    && tokio::time::Instant::now() >= consumed.deadline - Duration::from_secs(2)
                    && tokio::time::Instant::now() < consumed.deadline
                {
                    report.stalled_has = true;
                    let _ = (&mut stop_receiver).await;
                    break;
                }
                let blocked = report.consumed.is_some()
                    && matches!(blocker, ReadinessBlocker::Busy | ReadinessBlocker::Dead);
                if blocked {
                    report.blocked_replies += 1;
                }
                let alive = !(blocked && blocker == ReadinessBlocker::Dead);
                let ready = !blocked;
                // A cancelled client may close its probe while this response is
                // in flight. A disconnect must not kill the sole fixture server.
                let _ = stream
                    .get_mut()
                    .write_all(
                        format!(
                            "{{\"ok\":true,\"op\":\"has\",\"alive\":{alive},\"ready\":{ready}}}\n"
                        )
                        .as_bytes(),
                    )
                    .await;
            }
            if report.consumed.is_none() {
                report.consumed = consumed_receiver.try_recv().ok();
            }
            report
        });
        let mut payload = vec![b'x'; 384 * 1024];
        match blocker {
            ReadinessBlocker::WrongMark => payload.extend_from_slice(b"\x1b_cc-d-imark;{\"kind\":\"prompt_idle\",\"nonce\":\"fedcba9876543210fedcba9876543210\"}\x1b\\"),
            ReadinessBlocker::MissingMark => {},
            _ => payload.extend_from_slice(NONCE_MARK),
        }
        // No clear/home recovery point follows the replay. This is actual loss
        // of reconstruction, rather than a known readable draft/modal.
        payload.extend(paint("", PROMPT));
        let sent = attachment.get_mut().write_all(&payload).await;
        let mut observed = Vec::new();
        let read = tokio::time::timeout(
            Duration::from_secs(22),
            attachment.take(128).read_to_end(&mut observed),
        )
        .await;
        let _ = stop_sender.send(());
        let report = probes.await.expect("negative probe server");
        // Release/join all tasks before assertions, including on unexpected input.
        sent.expect("stream replay");
        read.expect("client closes attachment")
            .expect("observed input");
        (observed, report)
    });
    let verified = Arc::new(AtomicBool::new(false));
    let verification = Arc::clone(&verified);
    let result = tokio::time::timeout(
        Duration::from_secs(20),
        model_input(&endpoint, "01234567", "opus", move || {
            verification.store(true, Ordering::Release);
            if blocker == ReadinessBlocker::IdentityChanged {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "fixture native identity changed",
                ))
            } else {
                Ok(())
            }
        }),
    )
    .await;
    let client_completed = result.is_ok();
    let error = match result {
        Ok(Err(error)) => Some(error),
        Ok(Ok(stream)) => {
            drop(stream);
            None
        }
        Err(_) => None,
    };
    let (observed, report) = server.await.expect("negative readiness server");
    BlockedExchange {
        error,
        observed,
        verified: verified.load(Ordering::Acquire),
        client_completed,
        report,
    }
}

async fn assert_evicted_readiness_is_strict(blocker: ReadinessBlocker) -> BlockedExchange {
    let result = blocked_evicted_exchange(blocker).await;
    assert!(
        result.client_completed,
        "{blocker:?}: fixture deadline is not the tested error"
    );
    assert!(
        result.observed.is_empty(),
        "{blocker:?}: no model-command byte is admitted"
    );
    let consumed = result
        .report
        .consumed
        .expect("real scanner consumed eviction");
    assert!(
        consumed.bytes > 128 * 1024,
        "{blocker:?}: cannot pass before actual eviction"
    );
    let error = result.error.as_ref().expect("readiness remains blocked");
    assert!(
        !super::super::model_switch::is_requires_fresh(error),
        "{blocker:?}: strict readiness/identity error, not fresh compatibility: {error}"
    );
    if blocker != ReadinessBlocker::IdentityChanged {
        assert!(
            !result.verified,
            "{blocker:?}: identity is not sufficient readiness"
        );
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }
    result
}

#[tokio::test]
async fn in_flight_has_timeout_after_consumed_eviction_stays_strict() {
    let result = assert_evicted_readiness_is_strict(ReadinessBlocker::StalledHas).await;
    assert!(
        result.report.stalled_has,
        "a real HAS RPC stalled after actual consumed eviction"
    );
}

#[tokio::test]
async fn latest_busy_has_after_consumed_eviction_stays_strict() {
    let result = assert_evicted_readiness_is_strict(ReadinessBlocker::Busy).await;
    assert!(
        result.report.blocked_replies > 0,
        "latest known readiness was busy, not stale ready"
    );
}

#[tokio::test]
async fn latest_dead_has_after_consumed_eviction_stays_strict() {
    let result = assert_evicted_readiness_is_strict(ReadinessBlocker::Dead).await;
    assert!(
        result.report.blocked_replies > 0,
        "latest known readiness was dead, not stale ready"
    );
}

#[tokio::test]
async fn wrong_nonce_idle_mark_after_consumed_eviction_stays_strict() {
    assert_evicted_readiness_is_strict(ReadinessBlocker::WrongMark).await;
}

#[tokio::test]
async fn missing_idle_mark_after_consumed_eviction_stays_strict() {
    assert_evicted_readiness_is_strict(ReadinessBlocker::MissingMark).await;
}

#[tokio::test]
async fn changed_identity_before_evicted_screen_fallback_stays_strict() {
    let result = assert_evicted_readiness_is_strict(ReadinessBlocker::IdentityChanged).await;
    assert!(result.verified, "fallback must revalidate native identity");
    let error = result.error.expect("identity verifier error");
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(error.to_string(), "fixture native identity changed");
}
