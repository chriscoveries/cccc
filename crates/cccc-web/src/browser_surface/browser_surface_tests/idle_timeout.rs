use super::*;

/// A browser that has been silent for the timeout is retired, and its process goes
/// with it; the next use re-opens the surface on demand.
#[tokio::test]
async fn idle_browser_surface_is_closed_and_its_process_retired() {
    require_chrome!();
    let (url, server) = local_page("Idle browser surface").await;
    let temp = tempfile::tempdir().expect("tempdir");
    let manager = BrowserSurfaces::default();
    let key = "web-model::g_idle::reader";
    let profile = temp.path().join("profile");
    let opened = manager
        .open(key, &profile, &url, 800, 600)
        .await
        .expect("open fixture browser");
    let pid = opened["metadata"]["pid"].as_u64().expect("browser pid");
    assert!(pid > 0, "fixture browser must report its pid");

    // A browser that just spoke CDP is not idle, however generous the timeout.
    assert_eq!(
        manager
            .close_idle(std::time::Duration::from_secs(3600))
            .await
            .expect("idle sweep"),
        0
    );
    assert_eq!(manager.info(key).await["active"], true);

    // Once the silence reaches the timeout the sweep closes it and frees the process.
    assert_eq!(
        manager
            .close_idle(std::time::Duration::ZERO)
            .await
            .expect("idle sweep"),
        1
    );
    assert_eq!(manager.info(key).await["active"], false);
    wait_for_process_exit(pid).await;
    manager.shutdown_all().await.expect("shutdown");
    server.abort();
}

/// The process an Actor's window spawns must say whose it is, so an operator or a
/// reaper can attribute it without guessing from the profile path.
#[tokio::test]
async fn actor_browser_process_carries_its_actor_id() {
    require_chrome!();
    let (url, server) = local_page("Actor attribution").await;
    let temp = tempfile::tempdir().expect("tempdir");
    let manager = BrowserSurfaces::default();
    let key = "web-model::g_attr::reader";
    let profile = temp.path().join("profile");
    let opened = manager
        .open(key, &profile, &url, 800, 600)
        .await
        .expect("open fixture browser");
    let pid = opened["metadata"]["pid"].as_u64().expect("browser pid");
    if cfg!(target_os = "linux") {
        assert_eq!(
            process_env(pid, "CCCC_ACTOR_ID").as_deref(),
            Some("reader"),
            "browser process must carry the owning Actor id"
        );
    }
    manager.shutdown_all().await.expect("shutdown");
    server.abort();
}

#[cfg(target_os = "linux")]
fn process_env(pid: u64, name: &str) -> Option<String> {
    let environ = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    let needle = format!("{name}=");
    environ.split(|byte| *byte == 0).find_map(|entry| {
        std::str::from_utf8(entry)
            .ok()?
            .strip_prefix(&needle)
            .map(str::to_owned)
    })
}

#[cfg(not(target_os = "linux"))]
fn process_env(_pid: u64, _name: &str) -> Option<String> {
    None
}

async fn wait_for_process_exit(pid: u64) {
    #[cfg(target_os = "linux")]
    {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        while std::path::Path::new(&format!("/proc/{pid}")).exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "browser process {pid} outlived the idle close"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = pid;
}
