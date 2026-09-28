use super::*;

/// The production idle timeout, so the cutoff under test is the one the reaper uses.
const IDLE_TTL: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// A clock far enough past a surface's last CDP message to clear `IDLE_TTL`, so the
/// sweep is exercised with a positive TTL instead of `Duration::ZERO` (which closes
/// anything, whether or not the cutoff itself works).
fn aged_clock() -> std::time::Instant {
    std::time::Instant::now() + IDLE_TTL + std::time::Duration::from_secs(1)
}

/// A browser that has been silent past the timeout is retired, and its process goes
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

    // Silence that reaches the timeout retires it and frees the process.
    assert_eq!(
        manager
            .close_idle_at(IDLE_TTL, aged_clock())
            .await
            .expect("idle sweep"),
        1
    );
    assert_eq!(manager.info(key).await["active"], false);
    wait_for_process_exit(pid).await;
    manager.shutdown_all().await.expect("shutdown");
    server.abort();
}

/// The sweep is scoped to web-model Actor slots. A presentation surface is silenter
/// than any timeout while a person is watching it, so silence alone must not retire
/// it; its own route (or its group) closes it.
#[tokio::test]
async fn idle_sweep_leaves_non_web_model_surfaces_alone() {
    require_chrome!();
    let (url, server) = local_page("Idle sweep scope").await;
    let temp = tempfile::tempdir().expect("tempdir");
    let manager = BrowserSurfaces::default();
    let actor_key = "web-model::g_idle::reader";
    let presentation_key = "g_idle::presentation";
    let opened_actor = manager
        .open(actor_key, &temp.path().join("actor-profile"), &url, 800, 600)
        .await
        .expect("open actor browser");
    let actor_pid = opened_actor["metadata"]["pid"]
        .as_u64()
        .expect("actor browser pid");
    let opened_presentation = manager
        .open(
            presentation_key,
            &temp.path().join("presentation-profile"),
            &url,
            800,
            600,
        )
        .await
        .expect("open presentation browser");
    let presentation_pid = opened_presentation["metadata"]["pid"]
        .as_u64()
        .expect("presentation browser pid");
    assert_ne!(actor_pid, presentation_pid);

    assert_eq!(
        manager
            .close_idle_at(IDLE_TTL, aged_clock())
            .await
            .expect("idle sweep"),
        1,
        "only the web-model surface is swept"
    );
    assert_eq!(manager.info(actor_key).await["active"], false);
    assert_eq!(
        manager.info(presentation_key).await["active"],
        true,
        "a silent non-web-model surface outlives the idle sweep"
    );
    assert!(
        process_alive(presentation_pid),
        "the idle sweep must not retire a non-web-model browser process"
    );
    wait_for_process_exit(actor_pid).await;
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

#[cfg(target_os = "linux")]
fn process_alive(pid: u64) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(not(target_os = "linux"))]
fn process_alive(pid: u64) -> bool {
    let _ = pid;
    true
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

/// CCCC_BROWSER_IDLE_SECS is clamped, not taken literally: `0` - and unset, blank or
/// unparseable - means the default, never `Duration::ZERO`, which would retire every
/// surface on the next sweep; anything else below the floor is raised to it.
#[test]
fn browser_idle_timeout_clamps_the_environment_value() {
    use crate::browser_surface::{
        clamp_browser_idle_timeout, DEFAULT_BROWSER_IDLE_TIMEOUT, MIN_BROWSER_IDLE_TIMEOUT,
    };
    let default = DEFAULT_BROWSER_IDLE_TIMEOUT;
    let floor = MIN_BROWSER_IDLE_TIMEOUT;
    assert_eq!(floor, std::time::Duration::from_secs(60), "floor is 60s");
    assert_eq!(
        default,
        std::time::Duration::from_secs(15 * 60),
        "default is 15 minutes"
    );

    // Unset, blank, unparseable and 0 all mean the default.
    for raw in [
        None,
        Some(""),
        Some("   "),
        Some("0"),
        Some("0 "),
        Some("nope"),
        Some("-1"),
    ] {
        assert_eq!(
            clamp_browser_idle_timeout(raw),
            default,
            "raw {raw:?} must fall back to the default timeout"
        );
    }

    // Anything below the floor is raised to it, so a typo cannot retire live surfaces.
    assert_eq!(clamp_browser_idle_timeout(Some("1")), floor);
    assert_eq!(clamp_browser_idle_timeout(Some("59")), floor);

    // The floor itself and anything above it pass through unchanged.
    assert_eq!(clamp_browser_idle_timeout(Some("60")), floor);
    assert_eq!(
        clamp_browser_idle_timeout(Some("900")),
        std::time::Duration::from_secs(900)
    );
}
