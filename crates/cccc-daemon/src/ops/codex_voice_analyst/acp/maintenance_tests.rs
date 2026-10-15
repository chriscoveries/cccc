use super::*;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;

const SESSION: &str = "maintenance-session";

const PROVIDER: &str = r#"
import json, pathlib, sys
record = pathlib.Path(sys.argv[1])
load = prompt = None
def emit(frame):
    print(json.dumps({'jsonrpc':'2.0', **frame}), flush=True)
for line in sys.stdin:
    request = json.loads(line)
    with record.open('a') as log:
        log.write(json.dumps(request) + '\n')
    method = request.get('method')
    if method is None:
        continue
    if method == 'initialize':
        result = {'protocolVersion':1, 'agentCapabilities':{'loadSession':True}}
    elif method == 'session/new':
        result = {'sessionId':'maintenance-session'}
    elif method == 'session/load':
        load = request
        continue
    elif method == 'session/prompt':
        prompt = request
        continue
    else:
        result = {}
        if method == 'fixture/release-load':
            emit({'id':load['id'], 'result':{'sessionId':'maintenance-session'}})
            load = None
        elif method == 'fixture/admit':
            emit({'method':'session/update', 'params':{'sessionId':'maintenance-session',
                'update':{'sessionUpdate':'agent_message_chunk',
                          'content':{'type':'text', 'text':'working'}}}})
        elif method == 'fixture/permission':
            emit({'id':'permission', 'method':'session/request_permission',
                  'params':{'sessionId':'maintenance-session',
                            'toolCall':{'title':'fixture only'},
                            'options':[{'optionId':'once', 'kind':'allow_once'}]}})
        elif method == 'fixture/finish':
            emit({'id':prompt['id'], 'result':{'stopReason':'end_turn'}})
            prompt = None
    emit({'id':request['id'], 'result':result})
"#;

struct ProviderChild(Child);

impl Drop for ProviderChild {
    fn drop(&mut self) {
        // Covers setup failures and assertions, including pending RPCs: no
        // provider process or blocked stdout reader survives an early exit.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Fixture {
    protocol: Arc<AcpClient>,
    _child: ProviderChild,
    temp: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> Self {
        let temp = tempfile::tempdir().expect("private provider fixture");
        let mut child = ProviderChild(
            Command::new("python3")
                .args(["-u", "-c", PROVIDER])
                .arg(temp.path().join("requests.jsonl"))
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("fixture process"),
        );
        let protocol = AcpClient::new(
            child.0.stdin.take().expect("stdin"),
            child.0.stdout.take().expect("stdout"),
            "maintenance-generation".into(),
            "fixture",
            PermissionPolicy::Interactive,
            PromptCompletion::ResponseWithActivityReceipt,
        )
        .expect("protocol");
        let fixture = Self {
            protocol: Arc::new(protocol),
            _child: child,
            temp,
        };
        fixture.request("initialize").await;
        fixture.request("session/new").await;
        fixture
    }

    async fn request(&self, method: &str) -> Value {
        self.protocol
            .request(method, json!({}), Duration::from_secs(3))
            .await
            .expect("fixture RPC")
    }

    fn records(&self) -> Vec<Value> {
        std::fs::read_to_string(self.temp.path().join("requests.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("provider frame"))
            .collect()
    }

    async fn wait_for(&self, method: &str) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while !self.records().iter().any(|frame| frame["method"] == method) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("provider received request");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_close_retires_generation_and_rejects_later_prompts() {
    let fixture = Fixture::new().await;
    let auxiliary = tokio::spawn(std::future::pending::<()>());
    let auxiliary_abort = auxiliary.abort_handle();
    fixture.protocol.register_auxiliary_task(auxiliary);
    let mut events = fixture.protocol.subscribe();
    assert!(fixture.protocol.close_if_idle().await.expect("idle close"));
    let disconnected = tokio::time::timeout(Duration::from_secs(3), events.recv())
        .await
        .expect("normal close event")
        .expect("disconnect event");
    assert_eq!(disconnected.generation, "maintenance-generation");
    assert_eq!(
        disconnected.message["method"],
        MANAGED_AGENT_DISCONNECTED_METHOD
    );
    let error = fixture
        .protocol
        .start_prompt(SESSION, "too-late", "must never reach the provider")
        .await
        .expect_err("retired generation rejects admission");
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    assert!(
        fixture
            .protocol
            .request("fixture/barrier", json!({}), Duration::from_secs(3))
            .await
            .is_err()
    );
    assert!(
        fixture
            .records()
            .iter()
            .all(|frame| frame["method"] != "session/prompt")
    );
    tokio::time::timeout(Duration::from_secs(3), async {
        while !auxiliary_abort.is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("ordinary close aborts auxiliary tasks");
    // The coordinator uses the ordinary stop path after successful admission.
    fixture.protocol.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_close_preserves_external_busy_state_without_pending_rpcs() {
    let fixture = Fixture::new().await;
    let auxiliary = tokio::spawn(std::future::pending::<()>());
    let auxiliary_abort = auxiliary.abort_handle();
    fixture.protocol.register_auxiliary_task(auxiliary);
    let control = fixture.protocol.lifecycle_control();
    control
        .status(SESSION, true)
        .await
        .expect("external turn starts");
    assert!(
        !fixture
            .protocol
            .close_if_idle()
            .await
            .expect("external work is active")
    );
    assert!(
        !auxiliary_abort.is_finished(),
        "refusal must preserve observers"
    );
    fixture.request("fixture/barrier").await;
    control
        .status(SESSION, false)
        .await
        .expect("external turn completes");
    assert!(
        fixture
            .protocol
            .close_if_idle()
            .await
            .expect("external work settled")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_close_preserves_a_pending_session_load() {
    let fixture = Fixture::new().await;
    let protocol = Arc::clone(&fixture.protocol);
    let load = tokio::spawn(async move {
        protocol
            .request(
                "session/load",
                json!({"sessionId":SESSION}),
                Duration::from_secs(3),
            )
            .await
    });
    fixture.wait_for("session/load").await;
    assert!(
        !fixture
            .protocol
            .close_if_idle()
            .await
            .expect("load is pending")
    );
    fixture.request("fixture/release-load").await;
    assert_eq!(
        load.await.expect("load task").expect("preserved load")["sessionId"],
        SESSION
    );
    assert!(
        fixture
            .protocol
            .close_if_idle()
            .await
            .expect("loaded idle session")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_close_preserves_pending_admission_active_work_and_permission() {
    let fixture = Fixture::new().await;
    let protocol = Arc::clone(&fixture.protocol);
    let prompt = tokio::spawn(async move {
        protocol
            .start_prompt(SESSION, "owned-work", "fixture task")
            .await
    });
    fixture.wait_for("session/prompt").await;
    assert!(
        !prompt.is_finished(),
        "the provider has not admitted the prompt"
    );
    assert!(
        !fixture
            .protocol
            .close_if_idle()
            .await
            .expect("pending admission")
    );
    fixture.request("fixture/admit").await;
    prompt
        .await
        .expect("prompt task")
        .expect("admission survived");
    assert!(!fixture.protocol.close_if_idle().await.expect("active work"));
    fixture.request("fixture/permission").await;
    let permissions = fixture.protocol.permissions();
    assert_eq!(permissions.len(), 1);
    assert!(
        !fixture
            .protocol
            .close_if_idle()
            .await
            .expect("pending approval")
    );
    assert_eq!(fixture.protocol.permissions(), permissions);
    fixture
        .protocol
        .respond_permission(
            permissions[0]["request_id"]
                .as_str()
                .expect("permission token"),
            true,
        )
        .await
        .expect("approval survived");
    fixture.request("fixture/barrier").await;
    assert!(fixture.records().iter().any(|frame| {
        frame["id"] == "permission" && frame["result"]["outcome"]["outcome"] == "selected"
    }));
    fixture.request("fixture/finish").await;
    assert!(
        fixture
            .protocol
            .close_if_idle()
            .await
            .expect("completed work is idle")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_close_defers_for_native_input_and_pre_admission_cancellation() {
    let fixture = Fixture::new().await;
    fixture
        .protocol
        .register_native_input("native", "unsent input")
        .await
        .expect("register");
    assert!(
        !fixture
            .protocol
            .close_if_idle()
            .await
            .expect("registered native input")
    );
    fixture
        .protocol
        .forget_native_input("native")
        .await
        .expect("forget");
    assert_eq!(
        fixture
            .protocol
            .cancel_input(SESSION, Some("cancelled-before-start"))
            .await
            .expect("pre-cancel"),
        None
    );
    assert!(
        !fixture
            .protocol
            .close_if_idle()
            .await
            .expect("pending cancellation")
    );
    let error = fixture
        .protocol
        .start_prompt(SESSION, "cancelled-before-start", "cancelled task")
        .await
        .expect_err("cancelled input is not sent");
    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    assert!(
        fixture
            .protocol
            .close_if_idle()
            .await
            .expect("all pending work retired")
    );
    assert!(
        fixture
            .records()
            .iter()
            .all(|frame| frame["method"] != "session/prompt")
    );
}
