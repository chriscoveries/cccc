//! The shared browser desktop is a global login/maintenance surface. Actor
//! viewers only expose their fixed Page target, never this shared desktop.
use crate::{
    AppState,
    api::{ApiError, ApiResult, success},
};
use axum::{
    Json, Router,
    extract::{Extension, Query, State, ws::WebSocketUpgrade},
    response::Response,
    routing::{get, post},
};
use serde_json::{Value, json};

#[derive(Default, serde::Deserialize)]
struct QueryArgs {
    #[serde(default)]
    mode: String,
    #[serde(default)]
    provider: String,
    #[serde(default)]
    viewer_mode: String,
    #[serde(default)]
    browser_profile: String,
    #[serde(default)]
    inspect: bool,
}
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/web-model/shared-browser", get(info))
        .route("/api/v1/web-model/shared-browser/open", post(open))
        .route("/api/v1/web-model/shared-browser/close", post(close))
        .route("/api/v1/web-model/shared-browser/ws", get(upgrade))
}
pub(super) fn provider(value: &str) -> Result<&'static str, ApiError> {
    match value {
        "" | "chatgpt_web" => Ok("chatgpt_web"),
        _ => Err(ApiError::bad("Unsupported Web Model provider")),
    }
}
fn key(_provider: &str) -> &'static str {
    "web-model-login"
}
pub(super) fn profile(state: &AppState, provider: &str) -> std::path::PathBuf {
    state
        .home
        .root()
        .join("state/web_model_browser/_shared")
        .join(provider)
        .join("chrome_profile")
}
fn effective_name(name: &str) -> &str {
    if name.is_empty() { "_shared" } else { name }
}
fn login_key(provider: &str, name: &str) -> String {
    if name.is_empty() {
        key(provider).to_owned()
    } else {
        format!("web-model-login::{provider}::{name}")
    }
}
fn configured_profiles(state: &AppState) -> Result<std::collections::BTreeSet<String>, ApiError> {
    let store =
        cccc_core::GroupStore::new(state.home.clone()).map_err(|e| ApiError::bad(e.to_string()))?;
    let mut names = std::collections::BTreeSet::from(["_shared".to_owned()]);
    for row in store.list().map_err(|e| ApiError::bad(e.to_string()))? {
        let group = store
            .load(&row.group_id)
            .map_err(|e| ApiError::bad(e.to_string()))?;
        for actor in group
            .actors
            .iter()
            .filter(|a| a.runtime.is_browser_web_model())
        {
            cccc_core::actors::validate_browser_profile(&actor.browser_profile)
                .map_err(|e| ApiError::bad(e.to_string()))?;
            names.insert(cccc_core::actors::browser_profile(actor).to_owned());
        }
    }
    Ok(names)
}
fn selected_profile(
    state: &AppState,
    principal: &crate::auth::Principal,
    value: &str,
) -> Result<String, ApiError> {
    if !principal.current_admin(&state.home).unwrap_or(false) {
        return Err(ApiError::forbidden(
            "Browser login requires the instance administrator",
        ));
    }
    let name = cccc_core::actors::validate_browser_profile(value)
        .map_err(|e| ApiError::bad(e.to_string()))?;
    if !name.is_empty() && !configured_profiles(state)?.contains(&name) {
        return Err(ApiError::not_found(
            "Browser profile has no configured Web Model Actor",
        ));
    }
    Ok(name)
}
pub(super) fn profile_named(
    state: &AppState,
    provider: &str,
    value: &str,
) -> Result<std::path::PathBuf, ApiError> {
    let name = cccc_core::actors::validate_browser_profile(value)
        .map_err(|e| ApiError::bad(e.to_string()))?;
    if name.is_empty() {
        return Ok(profile(state, provider));
    }
    let mut path = state.home.root().to_path_buf();
    for component in [
        "state",
        "web_model_browser",
        name.as_str(),
        provider,
        "chrome_profile",
    ] {
        path.push(component);
        match std::fs::create_dir(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(ApiError::bad(e.to_string())),
        }
        let metadata =
            std::fs::symlink_metadata(&path).map_err(|e| ApiError::bad(e.to_string()))?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(ApiError::bad(
                "Named browser profile must use owned directories, not symlinks",
            ));
        }
    }
    Ok(path)
}
async fn payload(state: &AppState, provider: &str, inspect: bool, name: &str) -> ApiResult {
    let key = login_key(provider, name);
    let mut surface = state.browser_surfaces.info(&key).await;
    let readiness = if inspect && surface["active"] == true {
        state
            .browser_surfaces
            .prompt_readiness(&key)
            .await
            .map_err(|e| ApiError::bad(e.to_string()))?
    } else {
        surface["metadata"]["prompt_readiness"].clone()
    };
    if inspect {
        surface = state.browser_surfaces.info(&key).await;
    }
    Ok(success(
        json!({"browser_profiles":configured_profiles(state)?,"browser_surface":surface,"browser_session":{"browser_profile":effective_name(name),"account_label":effective_name(name),"account_identity":"unverified","active":surface["active"],"ready":readiness["ready"],"login_required":readiness["login_required"],"verification_required":readiness["verification_required"],"state":readiness["state"],"checked_at":readiness["checked_at"],"tab_url":surface["url"]}}),
    ))
}
async fn info(
    Extension(principal): Extension<crate::auth::Principal>,
    State(state): State<AppState>,
    Query(query): Query<QueryArgs>,
) -> ApiResult {
    let name = selected_profile(&state, &principal, &query.browser_profile)?;
    payload(&state, provider(&query.provider)?, query.inspect, &name).await
}
async fn open(
    Extension(principal): Extension<crate::auth::Principal>,
    State(state): State<AppState>,
    Query(query): Query<QueryArgs>,
    Json(_body): Json<Value>,
) -> ApiResult {
    let provider = provider(&query.provider)?;
    let name = selected_profile(&state, &principal, &query.browser_profile)?;
    state
        .browser_surfaces
        .ensure_open_shared_system(
            &login_key(provider, &name),
            &profile_named(&state, provider, &name)?,
            "https://chatgpt.com/",
            1366,
            900,
        )
        .await
        .map_err(|e| ApiError::bad(format!("{e:#}")))?;
    payload(&state, provider, false, &name).await
}
async fn close(
    Extension(principal): Extension<crate::auth::Principal>,
    State(state): State<AppState>,
    Query(query): Query<QueryArgs>,
) -> ApiResult {
    let provider = provider(&query.provider)?;
    let name = selected_profile(&state, &principal, &query.browser_profile)?;
    // The settings action closes the owned login window, not the shared
    // process. Actor windows and authenticated profile remain available.
    state
        .browser_surfaces
        .close(&login_key(provider, &name))
        .await
        .map_err(|e| ApiError::bad(e.to_string()))?;
    payload(&state, provider, false, &name).await
}
async fn upgrade(
    Extension(principal): Extension<crate::auth::Principal>,
    State(state): State<AppState>,
    Query(query): Query<QueryArgs>,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    if state.web_mode.is_read_only() {
        return Err(ApiError::forbidden(
            "Shared login is unavailable in read-only mode",
        ));
    }
    let provider = provider(&query.provider)?;
    let name = selected_profile(&state, &principal, &query.browser_profile)?;
    let key = login_key(provider, &name);
    Ok(ws.on_upgrade(move |socket| async move {
        if query.mode == "vnc" {
            crate::browser_surface::serve_vnc_socket(
                socket,
                &state.browser_surfaces,
                &key,
                state.shutdown.subscribe(),
            )
            .await;
        } else {
            crate::browser_surface::serve_socket(
                socket,
                &state.browser_surfaces,
                &key,
                &query.viewer_mode,
                state.shutdown.subscribe(),
            )
            .await;
        }
    }))
}

#[cfg(test)]
#[path = "web_model_shared_browser_profile_tests.rs"]
mod profile_tests;

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn closing_login_keeps_actor_page_draft_and_shared_login() {
        if crate::system_browser_path().is_none()
            || !std::path::Path::new("/usr/bin/Xvfb").is_file()
        {
            return;
        }
        let _chrome = crate::browser_surface::chrome_test_guard().await;
        let temp = tempfile::tempdir().expect("temp");
        let home = cccc_core::HomeLayout::from_path(temp.path().join("home")).expect("home");
        let (_, _, _, state) = crate::app_with_shutdown(
            home,
            tokio::sync::broadcast::channel(1).0,
            crate::WebMode::Normal,
            None,
            crate::LiveBinding {
                host: "127.0.0.1".into(),
                port: 0,
            },
            "login-close-fixture".into(),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listen");
        let url = format!("http://{}/", listener.local_addr().expect("address"));
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().fallback(|| async { axum::response::Html("<textarea></textarea>") }),
            )
            .await
        });
        let provider = "chatgpt_web";
        let profile = profile(&state, provider);
        state
            .browser_surfaces
            .ensure_open_shared_system(key(provider), &profile, &url, 800, 600)
            .await
            .expect("login");
        state
            .browser_surfaces
            .ensure_open_shared_actor("actor", &profile, &url, (800, 600), "gen")
            .await
            .expect("actor");
        let page = state
            .browser_surfaces
            .sessions
            .lock()
            .await
            .get("actor")
            .expect("actor")
            .page
            .clone();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if page
                    .evaluate("Boolean(document.querySelector('textarea'))")
                    .await
                    .ok()
                    .and_then(|r| r.into_value::<bool>().ok())
                    == Some(true)
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("fixture ready");
        page.evaluate("document.cookie='fixture_login=shared;Path=/';document.querySelector('textarea').value='keep Actor draft'").await.expect("fixture login and draft");
        let before = state.browser_surfaces.info("actor").await;
        let result = close(
            Extension(crate::auth::Principal {
                user_id: "local".into(),
                allowed_groups: vec![],
                is_admin: true,
                raw_token: String::new(),
            }),
            State(state.clone()),
            Query(QueryArgs {
                provider: provider.into(),
                ..Default::default()
            }),
        )
        .await
        .expect("close login route");
        assert_eq!(result.0["result"]["browser_surface"]["active"], false);
        let after = state.browser_surfaces.info("actor").await;
        assert_eq!(after["active"], true);
        assert_eq!(after["metadata"]["pid"], before["metadata"]["pid"]);
        assert!(page.evaluate("document.cookie.includes('fixture_login=shared') && document.querySelector('textarea').value==='keep Actor draft'").await.expect("Actor survives close").into_value::<bool>().expect("bool"));
        // Reopening the settings window must reuse the authenticated owner.
        let reopened = state
            .browser_surfaces
            .ensure_open_shared_system(key(provider), &profile, &url, 800, 600)
            .await
            .expect("reopen login");
        assert_eq!(reopened["metadata"]["pid"], before["metadata"]["pid"]);
        assert!(state.browser_surfaces.page_available("actor").await);
        state
            .browser_surfaces
            .close("actor")
            .await
            .expect("close fixture Actor");
        state
            .browser_surfaces
            .shutdown_all()
            .await
            .expect("cleanup");
        server.abort();
    }
}
