use super::*;
use cccc_core::HomeLayout;

#[tokio::test]
async fn idle_close_disabled_by_default() {
    require_chrome!();
    let (url, server) = local_page("Idle close disabled").await;
    let temp = tempfile::tempdir().expect("tempdir");
    let manager = BrowserSurfaces::default();
    let profile = temp.path().join("profile");
    manager
        .open("actor", &profile, &url, 800, 600)
        .await
        .expect("open");
    assert_eq!(manager.idle_close_seconds(), 0);
    if let Some(s) = manager.sessions.lock().await.get("actor") {
        s.last_use
            .store(0, std::sync::atomic::Ordering::Relaxed);
    }
    assert_eq!(manager.close_idle_pages().await, 0);
    assert_eq!(manager.info("actor").await["active"], true);
    manager.shutdown_all().await.expect("shutdown");
    server.abort();
}

#[tokio::test]
async fn busy_session_not_closed() {
    require_chrome!();
    let (url, server) = local_page("Busy not closed").await;
    let temp = tempfile::tempdir().expect("tempdir");
    let manager = BrowserSurfaces::default();
    manager.set_idle_close_seconds(1);
    let profile = temp.path().join("profile");
    manager
        .open("actor", &profile, &url, 800, 600)
        .await
        .expect("open");
    manager.busy_enter("actor").await;
    if let Some(s) = manager.sessions.lock().await.get("actor") {
        s.last_use
            .store(0, std::sync::atomic::Ordering::Relaxed);
    }
    assert_eq!(manager.close_idle_pages().await, 0);
    assert_eq!(manager.info("actor").await["active"], true);
    manager.busy_exit("actor").await;
    manager.shutdown_all().await.expect("shutdown");
    server.abort();
}

#[tokio::test]
async fn next_use_after_idle_close_recovers() {
    require_chrome!();
    let (url, server) = local_page("Recover after close").await;
    let temp = tempfile::tempdir().expect("tempdir");
    let manager = BrowserSurfaces::default();
    manager.set_idle_close_seconds(1);
    let profile = temp.path().join("profile");
    manager
        .open("actor", &profile, &url, 800, 600)
        .await
        .expect("open");
    if let Some(s) = manager.sessions.lock().await.get("actor") {
        s.last_use
            .store(0, std::sync::atomic::Ordering::Relaxed);
    }
    assert_eq!(manager.close_idle_pages().await, 1);
    let page = manager.page("actor").await.expect("recover");
    assert!(page.url().await.is_ok());
    manager.shutdown_all().await.expect("shutdown");
    server.abort();
}

#[tokio::test]
async fn setting_off_no_close() {
    require_chrome!();
    let (url, server) = local_page("Setting off").await;
    let temp = tempfile::tempdir().expect("tempdir");
    let manager = BrowserSurfaces::default();
    manager.set_idle_close_seconds(0);
    let profile = temp.path().join("profile");
    manager
        .open("actor", &profile, &url, 800, 600)
        .await
        .expect("open");
    if let Some(s) = manager.sessions.lock().await.get("actor") {
        s.last_use
            .store(0, std::sync::atomic::Ordering::Relaxed);
    }
    assert_eq!(manager.close_idle_pages().await, 0);
    manager.shutdown_all().await.expect("shutdown");
    server.abort();
}
