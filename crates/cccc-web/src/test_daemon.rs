use cccc_client::DaemonClient;
use cccc_contracts::DaemonRequest;
use cccc_core::HomeLayout;
use serde_json::Value;
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::process::{Child, Command};

const FIXTURE_HOME: &str = "CCCC_TEST_DAEMON_HOME";

// A real daemon owns process-global runtime registries and process protection.
// Give each Home its own daemon process while exercising the usual IPC paths.
pub(crate) struct TestDaemon {
    child: Child,
    client: DaemonClient,
    pid: u32,
}

impl TestDaemon {
    pub(crate) async fn start(home: &HomeLayout) -> Self {
        let mut child = Command::new(std::env::current_exe().expect("test executable"))
            .args(["--exact", "test_daemon::run", "--ignored", "--nocapture"])
            .env(FIXTURE_HOME, home.root())
            .env("CCCC_HOME", home.root())
            .env("CCCC_DAEMON_TRANSPORT", "tcp")
            .env("CCCC_DAEMON_HOST", "127.0.0.1")
            .env("CCCC_DAEMON_PORT", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn fixture daemon");
        let pid = child.id().expect("fixture daemon PID");
        let client = DaemonClient::new(home.clone());
        let ping = DaemonRequest {
            v: 1,
            op: "ping".into(),
            args: Default::default(),
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(exit) = child.try_wait().expect("fixture daemon status") {
                    panic!("fixture daemon exited before readiness: {exit}");
                }
                if client.call(&ping).await.is_ok_and(|reply| {
                    reply.ok
                        && reply.result.get("pid").and_then(Value::as_u64) == Some(u64::from(pid))
                }) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fixture daemon ready");
        Self { child, client, pid }
    }

    pub(crate) async fn stop(mut self) {
        let response = self
            .client
            .call(&DaemonRequest {
                v: 1,
                op: "shutdown".into(),
                args: [("expected_pid".into(), Value::from(self.pid))]
                    .into_iter()
                    .collect(),
            })
            .await
            .expect("fixture daemon shutdown");
        assert!(response.ok, "{:?}", response.error);
        let exit = tokio::time::timeout(Duration::from_secs(5), self.child.wait())
            .await
            .expect("fixture daemon shutdown timeout")
            .expect("fixture daemon exit");
        assert!(exit.success(), "fixture daemon exited with {exit}");
    }
}

#[tokio::test]
#[ignore = "daemon subprocess entry point"]
async fn run() {
    let Some(home) = std::env::var_os(FIXTURE_HOME) else {
        return;
    };
    let home = HomeLayout::from_path(PathBuf::from(home)).expect("fixture Home");
    cccc_daemon::run(home).await.expect("run fixture daemon");
}
