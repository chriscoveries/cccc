mod fresh_session_identity_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    const WORKER: &str = "ca52e69a-6596-4abd-a0ec-3e8690dc70e1";
    const SAVED: &str = "52b41c61-e23c-4b7c-8b60-809c347451b5";
    const SHORT: &str = "abcdef12";
    const GENERATION: &str = "12345678abcdef0012345678abcdef00";

    struct Fixture {
        _temp: tempfile::TempDir,
        _control: ControlDirectory,
        config: PathBuf,
        workspace: PathBuf,
        executable: PathBuf,
        saved: PathBuf,
        original: Vec<u8>,
        replies: Arc<std::sync::atomic::AtomicUsize>,
        kills: Arc<std::sync::atomic::AtomicUsize>,
        server: tokio::task::JoinHandle<()>,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.server.abort();
        }
    }

    impl Fixture {
        fn new(pre_named: bool, force_resume: bool) -> Self {
            let temp = tempfile::tempdir().expect("fixture directory");
            let workspace = temp.path().join("workspace");
            std::fs::create_dir(&workspace).expect("workspace");
            let (config, listener, control) = bind_fake_control(&temp.path().join("config"));
            std::fs::create_dir_all(config.join("projects/workspace")).expect("projects");
            std::fs::create_dir_all(config.join("jobs").join(SHORT)).expect("jobs");
            let saved = config
                .join("projects/workspace")
                .join(format!("{SAVED}.jsonl"));
            let original = format!(
                "{}\n",
                json!({"type":"custom-title", "sessionId":SAVED,
                    "customTitle":if pre_named { "cccc-actor-12345678" } else { "unrelated-session" }})
            )
            .into_bytes();
            std::fs::write(&saved, &original).expect("pre-named transcript");
            for (name, resumed) in [("fresh.json", false), ("resumed.json", true)] {
                cccc_core::fs::write_json(
                    &config.join(name),
                    &json!({
                        "sessionId":WORKER, "daemonShort":SHORT, "cwd":workspace,
                        "resumeSessionId":resumed.then_some(SAVED),
                        "tempo":"idle", "inFlight":{"tasks":0,"queued":0},
                        "intent":"", "outcome":null, "linkScanPath":null,
                        "linkScanOffset":0, "output":null, "tokens":null,
                    }),
                )
                .expect("startup state");
            }
            for (name, enabled) in [("pre-named", pre_named), ("force-resume", force_resume)] {
                if enabled {
                    std::fs::write(config.join(name), []).expect("fixture mode");
                }
            }
            let executable = temp.path().join("claude");
            std::fs::write(
                &executable,
                r#"#!/bin/sh
set -eu
if [ "${1-}" = '--version' ]; then
    printf '2.1.286 (Claude Code)\n'
    exit 0
fi
selected_name=
expect_name=0
for arg in "$@"; do
    if [ "$expect_name" = 1 ]; then
        selected_name=$arg
        expect_name=0
    elif [ "$arg" = '--name' ]; then
        expect_name=1
    fi
done
printf '%s\n' "$@" > "$CLAUDE_CONFIG_DIR/argv"
if [ -f "$CLAUDE_CONFIG_DIR/force-resume" ] ||
   { [ "$selected_name" = 'cccc-actor-12345678' ] && [ -f "$CLAUDE_CONFIG_DIR/pre-named" ]; }; then
    /bin/cp "$CLAUDE_CONFIG_DIR/resumed.json" "$CLAUDE_CONFIG_DIR/jobs/abcdef12/state.json"
else
    /bin/cp "$CLAUDE_CONFIG_DIR/fresh.json" "$CLAUDE_CONFIG_DIR/jobs/abcdef12/state.json"
fi
printf 'started · abcdef12\n'
"#,
            )
            .expect("fixture executable");
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
                .expect("executable permissions");
            let replies = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let kills = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let server_replies = Arc::clone(&replies);
            let server_kills = Arc::clone(&kills);
            let server_config = config.clone();
            let server_workspace = workspace.clone();
            let server = tokio::spawn(async move {
                let mut stopped = false;
                loop {
                    let (stream, _) = listener.accept().await.expect("control accept");
                    let mut stream = BufReader::new(stream);
                    let mut line = String::new();
                    stream.read_line(&mut line).await.expect("control read");
                    let request: Value = serde_json::from_str(&line).expect("control request");
                    let response = match request["op"].as_str().expect("operation") {
                        "list" => json!({"ok":true, "op":"list", "jobs":if stopped {
                            Vec::<Value>::new()
                        } else {
                            vec![json!({"short":SHORT,"sessionId":WORKER,
                                "cwd":server_workspace,"cliVersion":"2.1.286"})]
                        }}),
                        "reply" => {
                            server_replies.fetch_add(1, Ordering::SeqCst);
                            let state_path =
                                server_config.join("jobs").join(SHORT).join("state.json");
                            let mut state: Value = serde_json::from_slice(
                                &std::fs::read(&state_path).expect("job state"),
                            )
                            .expect("state JSON");
                            let id = state["resumeSessionId"].as_str().unwrap_or(WORKER);
                            let transcript = server_config
                                .join("projects/workspace")
                                .join(format!("{id}.jsonl"));
                            let records = [
                                json!({"type":"user","sessionId":id,"promptId":"first-turn",
                                    "message":{"content":request["text"]}}),
                                json!({"type":"assistant","sessionId":id,
                                    "message":{"content":[{"type":"text","text":"alive"}]}}),
                                json!({"type":"system","sessionId":id,"subtype":"turn_duration"}),
                            ];
                            use std::io::Write;
                            let mut output = std::fs::OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open(&transcript)
                                .expect("transcript");
                            for record in records {
                                writeln!(output, "{record}").expect("transcript record");
                            }
                            state["linkScanPath"] = json!(transcript);
                            cccc_core::fs::write_json(&state_path, &state)
                                .expect("publish transcript");
                            json!({"ok":true,"op":"reply"})
                        }
                        "kill" => {
                            server_kills.fetch_add(1, Ordering::SeqCst);
                            stopped = true;
                            json!({"ok":true,"op":"kill"})
                        }
                        op => panic!("unexpected operation {op}"),
                    };
                    stream
                        .get_mut()
                        .write_all(format!("{response}\n").as_bytes())
                        .await
                        .expect("control response");
                }
            });
            Self {
                _temp: temp,
                _control: control,
                config,
                workspace,
                executable,
                saved,
                original,
                replies,
                kills,
                server,
            }
        }

        async fn launch(&self) -> io::Result<LaunchedClaude> {
            launch(
                command::PreparedClaude {
                    executable: self.executable.to_string_lossy().into_owned(),
                    arguments: Vec::new(),
                    launch_environment: BTreeMap::from([(
                        "CLAUDE_CONFIG_DIR".into(),
                        self.config.to_string_lossy().into_owned(),
                    )]),
                    config_dir: self.config.clone(),
                    settings_path: self.config.join("managed.json"),
                },
                &self.workspace,
                GENERATION,
                SessionPurpose::Actor,
                None,
            )
            .await
        }

        async fn first_turn(&self) {
            let launched = self.launch().await.expect("fresh startup");
            assert!(!launched.resumed);
            let mut events = launched.protocol.subscribe();
            launched
                .protocol
                .start_prompt("first-turn", "say alive")
                .await
                .expect("first prompt accepted");
            let outcome = tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    let event = events.recv().await.expect("managed event");
                    if event.message["method"] == "turn/completed" {
                        break Ok(());
                    }
                    if event.message["method"] == MANAGED_AGENT_DISCONNECTED_METHOD {
                        break Err(event.message);
                    }
                }
            })
            .await;
            let alive = launched.protocol.running();
            let _ = launched.protocol.close().await;
            assert!(
                matches!(outcome, Ok(Ok(()))),
                "first turn failed: {outcome:?}"
            );
            assert!(alive, "healthy first turn must leave the lane running");
            assert_eq!(self.replies.load(Ordering::SeqCst), 1);
            assert_eq!(
                std::fs::read(&self.saved).expect("saved transcript"),
                self.original
            );
        }
    }

    #[tokio::test]
    async fn fresh_launch_with_a_pre_named_transcript_survives_its_first_turn() {
        Fixture::new(true, false).first_turn().await;
    }

    #[tokio::test]
    async fn unexpected_resumed_conversation_is_rejected_before_any_prompt() {
        let fixture = Fixture::new(true, true);
        let error = match fixture.launch().await {
            Err(error) => error,
            Ok(launched) => {
                let _ = launched.protocol.close().await;
                panic!("fresh launch accepted another conversation's resume identity");
            }
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("fresh launch"), "{error}");
        assert!(error.to_string().contains("resumed"), "{error}");
        assert_eq!(fixture.replies.load(Ordering::SeqCst), 0);
        assert_eq!(
            fixture.kills.load(Ordering::SeqCst),
            0,
            "an unexpectedly adopted conversation has no proven CCCC ownership"
        );
        assert_eq!(
            std::fs::read(&fixture.saved).expect("saved transcript"),
            fixture.original
        );
    }

    #[tokio::test]
    async fn fresh_launch_without_a_named_conversation_still_completes_a_turn() {
        Fixture::new(false, false).first_turn().await;
    }

    impl Fixture {
        async fn launch_with_late_identity(
            &self,
            resume_id: &str,
            tempo: &str,
        ) -> io::Result<LaunchedClaude> {
            let initial_path = self.config.join("fresh.json");
            let mut initial: Value =
                serde_json::from_slice(&std::fs::read(&initial_path)?).map_err(io::Error::other)?;
            initial["tempo"] = json!("active");
            initial["resumeSessionId"] = Value::Null;
            cccc_core::fs::write_json(&initial_path, &initial)?;
            let mut barrier = FreshSettleBarrier::register(&self.config, SHORT)?;
            let launch = self.launch();
            tokio::pin!(launch);
            tokio::select! {
                result = &mut launch => {
                    let detail = match result {
                        Ok(launched) => {
                            // Dropping the observer must not send an explicit
                            // stop to a job whose ownership is under test.
                            drop(launched);
                            "launch returned before the busy snapshot".to_owned()
                        }
                        Err(error) => format!("launch failed before the busy snapshot: {error}"),
                    };
                    return Err(io::Error::other(detail));
                }
                reached = barrier.reached() => reached?,
            }
            // The real launch has checked its initial fresh state and is now
            // held after reading a busy settlement snapshot. Publish a whole
            // new state before releasing it, without a timing-based sleep.
            let path = self.config.join("jobs").join(SHORT).join("state.json");
            let mut state: Value =
                serde_json::from_slice(&std::fs::read(&path)?).map_err(io::Error::other)?;
            state["resumeSessionId"] = json!(resume_id);
            state["tempo"] = json!(tempo);
            cccc_core::fs::write_json(&path, &state)?;
            barrier.release()?;
            tokio::time::timeout(LIVE_SETTLE_TIMEOUT * 2, &mut launch)
                .await
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        "fixture launch exceeded its own settlement deadline",
                    )
                })?
        }

        async fn reject_late_foreign_identity(&self, tempo: &str) {
            let result = self.launch_with_late_identity(SAVED, tempo).await;
            let error = match result {
                Err(error) => error,
                Ok(launched) => {
                    drop(launched);
                    panic!("fresh startup accepted a foreign resume published while settling");
                }
            };
            let endpoint = control::Endpoint::resolve(&self.config).expect("fixture endpoint");
            let jobs = control::list(&endpoint).await.expect("fixture job list");
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{error}");
            assert!(error.to_string().contains("fresh launch"), "{error}");
            assert!(error.to_string().contains("resumed"), "{error}");
            assert_eq!(self.replies.load(Ordering::SeqCst), 0);
            assert_eq!(
                self.kills.load(Ordering::SeqCst),
                0,
                "late foreign identity never establishes ownership for rollback"
            );
            assert_eq!(
                std::fs::read(&self.saved).expect("saved transcript"),
                self.original
            );
            assert!(
                jobs.iter()
                    .any(|job| job["short"] == SHORT && job["sessionId"] == WORKER),
                "the unowned native job remains listed"
            );
        }
    }

    #[tokio::test]
    async fn late_idle_foreign_resume_is_rejected_before_any_prompt() {
        Fixture::new(true, false)
            .reject_late_foreign_identity("idle")
            .await;
    }

    #[tokio::test]
    async fn late_active_foreign_resume_is_rejected_without_killing_the_job() {
        Fixture::new(true, false)
            .reject_late_foreign_identity("active")
            .await;
    }

    #[tokio::test]
    async fn late_same_worker_identity_still_completes_a_first_turn() {
        let fixture = Fixture::new(true, false);
        let launched = fixture
            .launch_with_late_identity(WORKER, "idle")
            .await
            .expect("same-worker identity is a fresh startup");
        assert_eq!(launched.session_id, WORKER);
        assert!(!launched.resumed);
        let mut events = launched.protocol.subscribe();
        launched
            .protocol
            .start_prompt("first-turn", "say alive")
            .await
            .expect("first prompt accepted");
        let outcome = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let event = events.recv().await.expect("managed event");
                if event.message["method"] == "turn/completed" {
                    break Ok(());
                }
                if event.message["method"] == MANAGED_AGENT_DISCONNECTED_METHOD {
                    break Err(event.message);
                }
            }
        })
        .await;
        let alive = launched.protocol.running();
        let replies = fixture.replies.load(Ordering::SeqCst);
        let kills_before_owned_cleanup = fixture.kills.load(Ordering::SeqCst);
        let saved = std::fs::read(&fixture.saved).expect("saved transcript");
        let cleanup = launched.protocol.close().await;
        assert!(
            matches!(outcome, Ok(Ok(()))),
            "first turn failed: {outcome:?}"
        );
        assert!(alive, "healthy same-worker identity remains running");
        assert_eq!(replies, 1);
        assert_eq!(kills_before_owned_cleanup, 0);
        assert_eq!(saved, fixture.original);
        cleanup.expect("explicit cleanup of the proven fresh job");
    }
    #[tokio::test]
    async fn identity_receipt_retarget_refuses_live_adopted_worker_without_registry() {
        let fixture = Fixture::new(true, true);
        std::fs::copy(
            fixture.config.join("resumed.json"),
            fixture.config.join("jobs").join(SHORT).join("state.json"),
        )
        .expect("publish worker W adopting S");
        assert!(
            !fixture.config.join("sessions").exists(),
            "no registry fallback in this control"
        );
        let result = retarget::ensure_available(&fixture.config, SAVED).await;
        fixture.server.abort();
        let error = result.expect_err("a live worker W holding S excludes explicit resume S");
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock, "{error}");
        assert_eq!(fixture.kills.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.replies.load(Ordering::SeqCst), 0);
        assert_eq!(
            std::fs::read(&fixture.saved).expect("retained history"),
            fixture.original
        );
    }
}

struct FreshSettlePending {
    token: u64,
    reached: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}

type FreshSettleKey = (PathBuf, String);

fn fresh_settle_barriers()
-> &'static Mutex<std::collections::HashMap<FreshSettleKey, FreshSettlePending>> {
    static BARRIERS: std::sync::OnceLock<
        Mutex<std::collections::HashMap<FreshSettleKey, FreshSettlePending>>,
    > = std::sync::OnceLock::new();
    BARRIERS.get_or_init(Default::default)
}

struct FreshSettleBarrier {
    key: FreshSettleKey,
    token: u64,
    reached: Option<oneshot::Receiver<()>>,
    release: Option<oneshot::Sender<()>>,
}

impl FreshSettleBarrier {
    fn register(config_dir: &Path, short: &str) -> io::Result<Self> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let token = NEXT.fetch_add(1, Ordering::Relaxed);
        let key = (config_dir.to_path_buf(), short.to_owned());
        let (reached_sender, reached) = oneshot::channel();
        let (release, release_receiver) = oneshot::channel();
        let mut barriers = fresh_settle_barriers()
            .lock()
            .map_err(|_| io::Error::other("fixture barrier registry poisoned"))?;
        let std::collections::hash_map::Entry::Vacant(entry) = barriers.entry(key.clone()) else {
            return Err(io::Error::other("fixture path already has a barrier owner"));
        };
        entry.insert(FreshSettlePending {
            token,
            reached: reached_sender,
            release: release_receiver,
        });
        Ok(Self {
            key,
            token,
            reached: Some(reached),
            release: Some(release),
        })
    }

    async fn reached(&mut self) -> io::Result<()> {
        let receiver = self
            .reached
            .take()
            .ok_or_else(|| io::Error::other("fixture busy observation may only be awaited once"))?;
        tokio::time::timeout(LAUNCH_TIMEOUT + JOB_READY_TIMEOUT, receiver)
            .await
            .map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "fixture busy snapshot not reached")
            })?
            .map_err(|_| io::Error::other("fixture busy observation was cancelled"))
    }

    fn release(&mut self) -> io::Result<()> {
        self.release
            .take()
            .ok_or_else(|| io::Error::other("fixture barrier was already released"))?
            .send(())
            .map_err(|_| io::Error::other("fixture launch stopped before release"))
    }
}

impl Drop for FreshSettleBarrier {
    fn drop(&mut self) {
        // Release an already-entered launch before attempting registry cleanup.
        // Cancellation or an assertion failure must never leave it waiting.
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
        if let Ok(mut barriers) = fresh_settle_barriers().lock()
            && barriers
                .get(&self.key)
                .is_some_and(|pending| pending.token == self.token)
        {
            barriers.remove(&self.key);
        }
    }
}

pub(super) async fn observe_fresh_settle_busy(config_dir: &Path, short: &str) {
    let pending = {
        let Ok(mut barriers) = fresh_settle_barriers().lock() else {
            return;
        };
        barriers.remove(&(config_dir.to_path_buf(), short.to_owned()))
    };
    if let Some(pending) = pending
        && pending.reached.send(()).is_ok()
    {
        let _ = pending.release.await;
    }
}
