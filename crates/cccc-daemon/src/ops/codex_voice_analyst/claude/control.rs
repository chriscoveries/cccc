use serde_json::{Value, json};
#[cfg(unix)]
use sha2::{Digest, Sha256};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const PROTOCOL_VERSION: u64 = 1;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_FRAME_BYTES: usize = 512 * 1024;

#[cfg(unix)]
pub(super) type PlatformStream = tokio::net::UnixStream;
#[cfg(windows)]
pub(super) type PlatformStream = tokio::net::windows::named_pipe::NamedPipeClient;

#[derive(Debug, Clone)]
pub(super) struct Endpoint {
    address: String,
    key_path: PathBuf,
}

impl Endpoint {
    #[cfg(test)]
    pub(super) fn fixture(address: String, key_path: PathBuf) -> Self {
        Self { address, key_path }
    }

    pub(super) fn resolve(config_dir: &Path) -> io::Result<Self> {
        let config_dir = config_dir.canonicalize()?;
        let key_path = config_dir.join("daemon/control.key");
        #[cfg(unix)]
        let address = {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            let config_metadata = std::fs::metadata(&config_dir)?;
            let digest = format!(
                "{:x}",
                Sha256::digest(config_dir.to_string_lossy().as_bytes())
            );
            let directory = Path::new("/tmp")
                .join(format!("cc-daemon-{}", config_metadata.uid()))
                .join(&digest[..8]);
            let metadata = std::fs::metadata(&directory).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!(
                        "Claude Agent View control directory is unavailable at {}: {error}",
                        directory.display()
                    ),
                )
            })?;
            if !metadata.is_dir()
                || metadata.uid() != config_metadata.uid()
                || metadata.permissions().mode() & 0o077 != 0
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Claude Agent View control directory failed ownership or permission validation",
                ));
            }
            directory
                .join("control.sock")
                .to_string_lossy()
                .into_owned()
        };
        #[cfg(windows)]
        let address = {
            let nonce_path = config_dir.join("daemon/pipe.key");
            let nonce = read_small_secret(&nonce_path, 16)?;
            if nonce.len() != 16 || !nonce.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Claude Agent View pipe nonce is invalid",
                ));
            }
            format!(r"\\.\pipe\cc-daemon-{}-control", nonce.to_ascii_lowercase())
        };
        Ok(Self { address, key_path })
    }

    fn auth(&self) -> io::Result<String> {
        let key = read_small_secret(&self.key_path, 128)?;
        if key.len() != 32 || !key.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Claude Agent View control key is invalid",
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let metadata = std::fs::symlink_metadata(&self.key_path)?;
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Claude Agent View control key must be a regular file",
                ));
            }
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Claude Agent View control key is accessible by another user",
                ));
            }
        }
        Ok(key)
    }

    pub(super) fn validate_credentials(&self) -> io::Result<()> {
        self.auth().map(drop)
    }
}

pub(super) async fn reply(endpoint: &Endpoint, short: &str, text: &str) -> io::Result<()> {
    let response = request(
        endpoint,
        json!({
            "proto":PROTOCOL_VERSION,
            "op":"reply",
            "short":short,
            "text":text,
            "auth":endpoint.auth()?,
        }),
    )
    .await?;
    require_ok(response, "reply")
}

pub(super) async fn kill(endpoint: &Endpoint, short: &str) -> io::Result<()> {
    let response = request(
        endpoint,
        json!({"proto":PROTOCOL_VERSION,"op":"kill","short":short,"signal":"SIGTERM"}),
    )
    .await?;
    if response.get("code").and_then(Value::as_str) == Some("ENOJOB") {
        return Ok(());
    }
    require_ok(response, "kill")
}

pub(super) async fn list(endpoint: &Endpoint) -> io::Result<Vec<Value>> {
    let response = request(endpoint, json!({"proto":PROTOCOL_VERSION,"op":"list"})).await?;
    if response.get("ok").and_then(Value::as_bool) != Some(true)
        || response.get("op").and_then(Value::as_str) != Some("list")
    {
        return Err(response_error(&response, "list"));
    }
    response
        .get("jobs")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Claude Agent View list omitted jobs",
            )
        })
}

/// Native terminal input runs local commands; queued `reply` text does not.
/// Only a nonce-bound native prompt marker proves that the resumed UI accepts input.
#[derive(Default)]
struct NativePromptMarks {
    pending: Vec<u8>,
    ready: bool,
}

impl NativePromptMarks {
    fn feed(&mut self, bytes: &[u8], nonce: &str) {
        const PREFIX: &[u8] = b"\x1b_cc-d-imark;";
        const END: &[u8] = b"\x1b\\";
        self.pending.extend_from_slice(bytes);
        loop {
            let Some(start) = self
                .pending
                .windows(PREFIX.len())
                .position(|part| part == PREFIX)
            else {
                let keep = self.pending.len().saturating_sub(PREFIX.len() - 1);
                self.pending.drain(..keep);
                return;
            };
            self.pending.drain(..start);
            let Some(end) = self.pending[PREFIX.len()..]
                .windows(END.len())
                .position(|part| part == END)
            else {
                if self.pending.len() > PREFIX.len() + 512 {
                    self.pending.drain(..PREFIX.len());
                    continue;
                }
                return;
            };
            let end = PREFIX.len() + end;
            if end - PREFIX.len() <= 512
                && let Ok(mark) = serde_json::from_slice::<Value>(&self.pending[PREFIX.len()..end])
                && mark.get("kind").and_then(Value::as_str) == Some("prompt_idle")
                && mark.get("nonce").and_then(Value::as_str) == Some(nonce)
            {
                self.ready = true;
            }
            self.pending.drain(..end + END.len());
        }
    }
}

pub(super) fn validate_model(model: &str) -> io::Result<()> {
    if model.is_empty()
        || model.len() > 200
        || !model
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:/[]".contains(&byte))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Claude model switch could not be confirmed: invalid model identifier",
        ));
    }
    Ok(())
}

pub(super) async fn model_input(
    endpoint: &Endpoint,
    short: &str,
    model: &str,
    verify_session: impl FnOnce() -> io::Result<()>,
) -> io::Result<PlatformStream> {
    validate_model(model)?;
    let mut stream = connect(endpoint).await?;
    write_frame(
        &mut stream,
        &json!({
            "proto":PROTOCOL_VERSION, "op":"attach", "short":short, "auth":endpoint.auth()?,
            "cols":120, "rows":40,
            "attachId":format!("cccc-model-{}", uuid::Uuid::new_v4().simple()),
            "caps":{"ssh":false, "colorLevel":0, "terminal":null, "mux":null, "imark":true},
        }),
    )
    .await?;
    let response = tokio::time::timeout(REQUEST_TIMEOUT, read_frame(&mut stream))
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "Claude model switch attach timed out",
            )
        })??;
    require_ok(response.clone(), "attach")?;
    let nonce = response.get("imarkNonce").and_then(Value::as_str)
        .filter(|nonce| nonce.len() == 32 && nonce.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported,
            "Claude model switch could not be confirmed: native prompt readiness is unsupported"))?;
    let mut marks = NativePromptMarks::default();
    let mut screen = super::cache_confirmation::Screen::default();
    // Attach acknowledges the supervisor connection before a resumed worker is
    // ready. Native writes made during that interval can be dropped.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut output = [0_u8; 8192];
    loop {
        let state = tokio::time::timeout_at(
            deadline,
            request(
                endpoint,
                json!({"proto":PROTOCOL_VERSION,"op":"has","short":short}),
            ),
        )
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "Claude model switch could not be confirmed: worker did not become ready",
            )
        })??;
        require_ok(state.clone(), "has")?;
        if state.get("alive").and_then(Value::as_bool) == Some(true)
            && state.get("ready").and_then(Value::as_bool) == Some(true)
            && marks.ready
            && screen.empty_prompt()
        {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Claude model switch could not be confirmed: worker did not become ready",
            ));
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(100)) => {},
            read = stream.read(&mut output) => {
                let count = read?;
                if count == 0 { return Err(io::Error::new(io::ErrorKind::BrokenPipe,
                    "Claude model switch could not be confirmed: worker attachment closed")); }
                marks.feed(&output[..count], nonce);
                screen.feed(&output[..count])?;
            }
        }
    }
    verify_session()?;
    stream
        .write_all(format!("/model {model}\r").as_bytes())
        .await?;
    stream.flush().await?;
    Ok(stream)
}

pub(super) async fn interrupt(endpoint: &Endpoint, short: &str) -> io::Result<()> {
    let mut stream = connect(endpoint).await?;
    write_frame(
        &mut stream,
        &json!({
            "proto":PROTOCOL_VERSION,
            "op":"attach",
            "short":short,
            "auth":endpoint.auth()?,
            "cols":120,
            "rows":40,
            "attachId":format!("cccc-cancel-{}", uuid::Uuid::new_v4().simple()),
            "caps":{"ssh":false,"colorLevel":0},
        }),
    )
    .await?;
    let response = tokio::time::timeout(REQUEST_TIMEOUT, read_frame(&mut stream))
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "Claude Agent View attach timed out",
            )
        })??;
    require_ok(response, "attach")?;
    stream.write_all(&[0x1b]).await?;
    stream.flush().await?;
    tokio::time::sleep(Duration::from_millis(30)).await;
    Ok(())
}

async fn request(endpoint: &Endpoint, value: Value) -> io::Result<Value> {
    let mut stream = connect(endpoint).await?;
    write_frame(&mut stream, &value).await?;
    tokio::time::timeout(REQUEST_TIMEOUT, read_frame(&mut stream))
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "Claude Agent View request timed out",
            )
        })?
}

#[cfg(unix)]
async fn connect(endpoint: &Endpoint) -> io::Result<PlatformStream> {
    tokio::time::timeout(
        CONNECT_TIMEOUT,
        tokio::net::UnixStream::connect(&endpoint.address),
    )
    .await
    .map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "Claude Agent View control connection timed out",
        )
    })?
}

#[cfg(windows)]
async fn connect(endpoint: &Endpoint) -> io::Result<PlatformStream> {
    let deadline = tokio::time::Instant::now() + CONNECT_TIMEOUT;
    loop {
        match tokio::net::windows::named_pipe::ClientOptions::new().open(&endpoint.address) {
            Ok(stream) => return Ok(stream),
            Err(error) if tokio::time::Instant::now() < deadline => {
                if !matches!(error.raw_os_error(), Some(2) | Some(231)) {
                    return Err(error);
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => return Err(error),
        }
    }
}

async fn write_frame(stream: &mut (impl AsyncWrite + Unpin), value: &Value) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    bytes.push(b'\n');
    stream.write_all(&bytes).await?;
    stream.flush().await
}

async fn read_frame(stream: &mut (impl AsyncRead + Unpin)) -> io::Result<Value> {
    let mut bytes = Vec::new();
    loop {
        let byte = stream.read_u8().await?;
        if byte == b'\n' {
            break;
        }
        bytes.push(byte);
        if bytes.len() > MAX_FRAME_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Claude Agent View control frame exceeded its limit",
            ));
        }
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn require_ok(response: Value, operation: &str) -> io::Result<()> {
    if response.get("ok").and_then(Value::as_bool) == Some(true)
        && response.get("op").and_then(Value::as_str) == Some(operation)
    {
        return Ok(());
    }
    Err(response_error(&response, operation))
}

fn response_error(response: &Value, operation: &str) -> io::Error {
    let code = response
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or("EUNKNOWN");
    let message = response
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("unrecognized control response");
    let kind = match code {
        "ENOREPLY" | "ERESPAWNING" | "ESTARTING" => io::ErrorKind::WouldBlock,
        "ENOJOB" | "EHOSTDEAD" => io::ErrorKind::ConnectionAborted,
        "EAUTH" => io::ErrorKind::PermissionDenied,
        "EPROTO" => io::ErrorKind::Unsupported,
        "ETIMEOUT" => io::ErrorKind::TimedOut,
        _ => io::ErrorKind::Other,
    };
    io::Error::new(
        kind,
        format!("Claude Agent View {operation} failed ({code}): {message}"),
    )
}

fn read_small_secret(path: &Path, max_bytes: u64) -> io::Result<String> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "refusing invalid Claude Agent View credential file: {}",
                path.display()
            ),
        ));
    }
    Ok(std::fs::read_to_string(path)?.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_marks_ignore_stale_nonce_and_handle_split_terminal_frames() {
        let mut marks = NativePromptMarks::default();
        marks.feed(
            b"\x1b_cc-d-imark;{\"kind\":\"prompt_idle\",\"nonce\":\"stale\"}\x1b\\",
            "current",
        );
        assert!(!marks.ready);
        marks.feed(b"terminal repaint\x1b_cc-d-im", "current");
        marks.feed(
            b"ark;{\"kind\":\"prompt_idle\",\"nonce\":\"current\"}\x1b",
            "current",
        );
        assert!(!marks.ready);
        marks.feed(b"\\", "current");
        assert!(marks.ready);
        let mut junk = NativePromptMarks::default();
        junk.feed(&vec![b'x'; 8192], "current");
        assert!(junk.pending.len() < 32);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn model_command_waits_for_native_worker_readiness() {
        use std::os::unix::fs::PermissionsExt;
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        let temp = tempfile::tempdir().expect("fixture directory");
        let address = temp.path().join("control.sock");
        let key_path = temp.path().join("control.key");
        std::fs::write(&key_path, "0123456789abcdef0123456789abcdef").expect("control key");
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))
            .expect("key permissions");
        let listener = tokio::net::UnixListener::bind(&address).expect("listener");
        let endpoint = Endpoint {
            address: address.to_string_lossy().into_owned(),
            key_path,
        };
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("native attachment");
            let mut stream = BufReader::new(stream);
            let mut line = String::new();
            stream.read_line(&mut line).await.expect("attach request");
            let request: Value = serde_json::from_str(&line).expect("request JSON");
            assert_eq!(request["op"], "attach");
            assert!(request["caps"]["terminal"].is_null());
            assert!(request["caps"]["mux"].is_null());
            stream
                .get_mut()
                .write_all(b"{\"ok\":true,\"op\":\"attach\",\"imarkNonce\":\"0123456789abcdef0123456789abcdef\"}\n")
                .await
                .expect("attach ack");
            for ready in [false, true] {
                let (has, _) = listener.accept().await.expect("readiness probe");
                let mut has = BufReader::new(has);
                line.clear();
                has.read_line(&mut line).await.expect("has request");
                let request: Value = serde_json::from_str(&line).expect("has JSON");
                assert_eq!(request["op"], "has");
                if !ready {
                    let mut byte = [0];
                    assert!(
                        tokio::time::timeout(Duration::from_millis(30), stream.read(&mut byte))
                            .await
                            .is_err(),
                        "no terminal command may precede worker readiness"
                    );
                }
                has.get_mut()
                    .write_all(
                        format!(
                            "{{\"ok\":true,\"op\":\"has\",\"alive\":true,\"ready\":{ready}}}\n"
                        )
                        .as_bytes(),
                    )
                    .await
                    .expect("readiness response");
            }
            let mut byte = [0];
            assert!(
                tokio::time::timeout(Duration::from_millis(30), stream.read(&mut byte))
                    .await
                    .is_err(),
                "worker readiness alone does not prove the prompt accepts input"
            );
            stream.get_mut().write_all("\x1b_cc-d-imark;{\"kind\":\"prompt_idle\",\"nonce\":\"0123456789abcdef0123456789abcdef\"}\x1b\\\x1b[?2026h\x1b[2J\x1b[H❯\r\n⏵⏵ bypass permissions on (shift+tab to cycle)\x1b[?2026l".as_bytes())
                .await.expect("native prompt marker");
            let (has, _) = listener.accept().await.expect("confirmed readiness probe");
            let mut has = BufReader::new(has);
            line.clear();
            has.read_line(&mut line).await.expect("final has request");
            has.get_mut()
                .write_all(b"{\"ok\":true,\"op\":\"has\",\"alive\":true,\"ready\":true}\n")
                .await
                .expect("final readiness response");
            let mut command = [0_u8; 12];
            stream
                .read_exact(&mut command)
                .await
                .expect("native model command");
            assert_eq!(&command, b"/model opus\r");
        });
        let stream = model_input(&endpoint, "01234567", "opus", || Ok(()))
            .await
            .expect("model input");
        server.await.expect("server task");
        drop(stream);
    }

    #[test]
    fn provider_busy_is_retryable_but_protocol_drift_is_not() {
        assert_eq!(
            response_error(&json!({"code":"ENOREPLY","error":"busy"}), "reply").kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(
            response_error(&json!({"code":"EPROTO","error":"changed"}), "reply").kind(),
            io::ErrorKind::Unsupported
        );
    }

    #[cfg(unix)]
    #[test]
    fn credential_validation_rejects_a_group_readable_key() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("tempdir");
        let key_path = temp.path().join("control.key");
        std::fs::write(&key_path, "0123456789abcdef0123456789abcdef\n").expect("key");
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o640))
            .expect("permissions");
        let endpoint = Endpoint {
            address: String::new(),
            key_path,
        };

        assert_eq!(
            endpoint
                .validate_credentials()
                .expect_err("insecure key")
                .kind(),
            io::ErrorKind::PermissionDenied
        );
    }
}
