//! A real stdio ACP boundary used by supervisor delivery/lifecycle tests.
use super::*;
use std::path::Path;
use std::time::Duration;

impl AnalystSession {
    pub(crate) async fn acp_for_headless_test(root: &Path) -> io::Result<Self> {
        let script = root.join("provider.py");
        std::fs::write(
            &script,
            r#"
import json, pathlib, sys, threading, time
root = pathlib.Path(sys.argv[1])
lock = threading.Lock()
cancel = threading.Event()
def send(value):
    with lock:
        print(json.dumps(value), flush=True)
def prompt(request):
    text = request['params']['prompt'][0]['text']
    if 'AMBIGUOUS' in text:
        send({'jsonrpc':'2.0','id':request['id'],'error':{'code':-32603,'message':'uncertain provider failure ' + 'λ' * 10000}})
        return
    if 'RETRY' in text and not (root / 'rejected').exists():
        (root / 'rejected').write_text('')
        send({'jsonrpc':'2.0','id':request['id'],'error':{'code':-32602,'message':'fixture pre-admission rejection'}})
        return
    if 'EXECBUSY' in text:
        # Sloppy provider: it records (executes) the prompt, then reports
        # busyness instead of a result. At-most-once forbids resubmission.
        with (root / 'prompts.jsonl').open('a') as file:
            file.write(json.dumps(text) + '\n')
        send({'jsonrpc':'2.0','id':request['id'],'error':{'code':-32603,'message':'provider already processing previous prompt'}})
        return
    with (root / 'prompts.jsonl').open('a') as file:
        file.write(json.dumps(text) + '\n')
    send({'jsonrpc':'2.0','method':'session/update','params':{
        'sessionId':'headless-fixture','update':{
            'sessionUpdate':'user_message_chunk','content':{'type':'text','text':text}}}})
    if 'FIRST' in text:
        while not (root / 'finish-first').exists() and not cancel.is_set():
            time.sleep(.01)
    if 'HOLD' in text:
        while not (root / 'finish-hold').exists() and not cancel.is_set():
            time.sleep(.01)
    send({'jsonrpc':'2.0','id':request['id'],'result':{'stopReason':'cancelled' if cancel.is_set() else 'end_turn'}})
for line in sys.stdin:
    request = json.loads(line)
    method = request['method']
    if method == 'session/prompt':
        threading.Thread(target=prompt, args=(request,), daemon=True).start()
        continue
    if method == 'session/cancel':
        cancel.set()
        continue
    result = {'sessionId':'headless-fixture'} if method in ['session/new','session/load'] else {'protocolVersion':1}
    if 'id' in request:
        send({'jsonrpc':'2.0','id':request['id'],'result':result})
"#,
        )?;
        let command = vec![
            "python3".into(),
            script.to_string_lossy().into_owned(),
            root.to_string_lossy().into_owned(),
        ];
        let (owner, stdin, stdout) =
            process::spawn_piped(&command, root, &BTreeMap::new(), "headless-fixture")?;
        let generation = uuid::Uuid::new_v4().to_string();
        let protocol = AcpClient::new(
            stdin,
            stdout,
            generation.clone(),
            "kilo",
            acp::PermissionPolicy::Reject,
            acp::PromptCompletion::Response,
        )?;
        protocol
            .request(
                "initialize",
                serde_json::json!({"protocolVersion":1}),
                Duration::from_secs(5),
            )
            .await?;
        protocol
            .request(
                "session/new",
                serde_json::json!({"cwd":root,"mcpServers":[]}),
                Duration::from_secs(5),
            )
            .await?;
        Ok(Self {
            binding: WorkspaceBinding {
                root: root.to_owned(),
            },
            generation,
            runtime: ActorRuntime::Kilo,
            endpoint: String::new(),
            thread_id: "headless-fixture".into(),
            remote_tui_prefix: Vec::new(),
            environment: BTreeMap::new(),
            protocol: ManagedProtocol::Acp(protocol),
            process: Some(Arc::new(owner)),
            auxiliary_processes: Vec::new(),
            native_tui_command: Some(vec![
                "sh".into(),
                "-c".into(),
                "stty -echo; printf '\\033[?2004h'; exec cat".into(),
            ]),
            cleanup_paths: Vec::new(),
            thread_resumed: false,
            delegations: tokio::sync::Mutex::new(HashMap::new()),
        })
    }
}

#[tokio::test]
async fn headless_receipt_capacity_queues_without_evicting_uncertain_admission() {
    let temp = tempfile::tempdir().expect("fixture");
    let app = AnalystSession::acp_for_headless_test(temp.path())
        .await
        .expect("ACP");
    {
        let mut receipts = app.delegations.lock().await;
        for index in 0..256 {
            receipts.insert(
                format!("pending-{index}"),
                DelegationState::Unresolved("lost admission".into()),
            );
        }
    }
    assert_eq!(
        app.start_delivery("next", "hello")
            .await
            .expect_err("must queue")
            .kind(),
        io::ErrorKind::WouldBlock
    );
    assert_eq!(app.delivery_receipt_count().await, 256);
    assert_eq!(
        app.start_delivery("pending-0", "hello")
            .await
            .expect_err("never replay")
            .kind(),
        io::ErrorKind::AlreadyExists
    );
    assert!(!temp.path().join("prompts.jsonl").exists());
    app.stop(app.generation()).await.expect("stop");
}

#[tokio::test]
async fn flag_unset_legacy_turn_keeps_rejected_admission_fenced() {
    let temp = tempfile::tempdir().expect("fixture");
    let app = AnalystSession::acp_for_headless_test(temp.path())
        .await
        .expect("ACP");
    let error = app
        .start_turn(app.generation(), "legacy", "RETRY")
        .await
        .expect_err("rejected before admission");
    assert_eq!(error.kind(), io::ErrorKind::Other);
    assert!(error.to_string().starts_with("ACP request failed: "));
    assert_eq!(app.delivery_receipt_count().await, 1);
    assert_eq!(
        app.start_turn(app.generation(), "legacy", "RETRY")
            .await
            .expect_err("legacy no-replay fence")
            .kind(),
        io::ErrorKind::AlreadyExists
    );
    assert!(!temp.path().join("prompts.jsonl").exists());
    app.stop(app.generation()).await.expect("stop");
}
