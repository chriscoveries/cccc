mod auth_support;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use cccc_contracts::DaemonRequest;
use cccc_core::HomeLayout;
use http_body_util::BodyExt;
use serde_json::{Map, Value};
use tower::ServiceExt;

#[tokio::test]
async fn anonymous_health_fails_when_the_daemon_is_unavailable() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");

    let response = cccc_web::app(home)
        .oneshot(
            Request::get("/api/v1/health")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let payload = response_json(response).await;
    assert_eq!(payload["error"]["code"], "daemon_unavailable");

    let response = cccc_web::app(HomeLayout::from_path(temp.path().join("home")).expect("home"))
        .oneshot(
            Request::get("/api/v1/ping")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn anonymous_health_checks_the_daemon_without_disclosing_details() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    let daemon_home = home.clone();
    let daemon = tokio::spawn(async move { cccc_daemon::run(daemon_home).await });
    wait_for_daemon(&home).await;

    let response = cccc_web::app(home.clone())
        .oneshot(
            Request::get("/api/v1/health")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::OK);
    let payload = response_json(response).await;
    assert_eq!(payload["result"]["status"], "ok");
    assert!(payload["result"]["disk"].is_null());
    assert!(payload["result"]["pid"].is_null());
    assert!(payload["result"]["build"].is_null());
    assert!(payload["result"]["executable"].is_null());

    let _ = cccc_client::DaemonClient::new(home)
        .call(&DaemonRequest {
            v: 1,
            op: "shutdown".into(),
            args: Map::new(),
        })
        .await;
    daemon.await.expect("daemon task").expect("daemon");
}

async fn response_json(response: axum::response::Response) -> Value {
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    serde_json::from_slice(&body).expect("json")
}

async fn wait_for_daemon(home: &HomeLayout) {
    for _ in 0..100 {
        if home.daemon_dir().join("ccccd.addr.json").exists() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("daemon address was not created");
}

#[tokio::test]
async fn authenticated_health_exposes_home_pressure_and_startup_publishes_one_global_crossing() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    home.initialize().expect("initialize");
    cccc_core::settings::update(&home, |settings| {
        settings.observability.insert(
            "disk_health".into(),
            serde_json::json!({"minimum_available_bytes":u64::MAX}),
        );
        Ok(())
    })
    .expect("force pressure using fixture configuration, not disk consumption");
    let daemon_home = home.clone();
    let daemon = tokio::spawn(async move { cccc_daemon::run(daemon_home).await });
    wait_for_daemon(&home).await;
    let client = cccc_client::DaemonClient::new(home.clone());
    let events = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let response = client
                .call(&DaemonRequest {
                    v: 1,
                    op: "disk_events".into(),
                    args: Map::new(),
                })
                .await
                .expect("events IPC");
            assert!(response.ok, "disk event operation unavailable");
            if !response.result["events"]
                .as_array()
                .expect("event list")
                .is_empty()
            {
                break response;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("background service must publish without a health request");
    assert_eq!(
        events.result["events"]
            .as_array()
            .expect("event list")
            .len(),
        1
    );
    assert_eq!(events.result["events"][0]["kind"], "disk.threshold_crossed");
    assert_eq!(events.result["events"][0]["severity"], "warning");
    let response = auth_support::authenticated_app(home.clone())
        .oneshot(
            Request::get("/api/v1/health")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let payload = response_json(response).await;
    assert_eq!(payload["result"]["disk"]["scope"], "cccc_home");
    assert_eq!(payload["result"]["disk"]["severity"], "warning");
    assert!(
        payload["result"]["disk"]["available_bytes"]
            .as_u64()
            .is_some()
    );
    assert!(payload["result"].get("executable").is_none());
    assert_eq!(
        std::fs::read_dir(home.groups_dir())
            .expect("valid test fixture")
            .count(),
        0
    );
    client
        .call(&DaemonRequest {
            v: 1,
            op: "shutdown".into(),
            args: Map::new(),
        })
        .await
        .expect("shutdown");
    daemon.await.expect("daemon task").expect("daemon");
}
