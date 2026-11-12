use super::*;

fn captured_state() -> (tempfile::TempDir, AppState) {
    let temp = tempfile::tempdir().expect("temporary home");
    let home = cccc_core::HomeLayout::from_path(temp.path().join("home")).expect("home");
    home.initialize().expect("initialize");
    let mut group: cccc_core::GroupDoc = serde_json::from_str(include_str!(
        "../../../cccc-core/tests/fixtures/browser-profile-group.json"
    ))
    .expect("retained metadata capture");
    // Named-selection control over captured metadata, not a second-account proof.
    group.running = false;
    let actor = group
        .actors
        .iter_mut()
        .find(|a| a.id == "web1")
        .expect("actor");
    actor.browser_profile = "secondary".into();
    actor.enabled = false;
    group.extra.remove("im"); // No external IM worker in this metadata control.
    cccc_core::GroupStore::new(home.clone())
        .expect("store")
        .import(group)
        .expect("import");
    let (_, _, _, state) = crate::app_with_shutdown(
        home,
        tokio::sync::broadcast::channel(1).0,
        crate::WebMode::Normal,
        None,
        crate::LiveBinding {
            host: "127.0.0.1".into(),
            port: 0,
        },
        "profile-tests".into(),
    );
    (temp, state)
}

fn administrator() -> crate::auth::Principal {
    crate::auth::Principal {
        user_id: "local".into(),
        allowed_groups: vec![],
        is_admin: true,
        raw_token: String::new(),
    }
}

#[tokio::test]
async fn named_login_selection_is_registered_admin_only_and_status_names_its_profile() {
    let (_temp, state) = captured_state();
    assert_eq!(
        selected_profile(&state, &administrator(), "_shared").expect("default"),
        ""
    );
    assert_eq!(
        selected_profile(&state, &administrator(), "secondary").expect("named"),
        "secondary"
    );
    assert!(selected_profile(&state, &administrator(), "unknown").is_err());
    let mut denied = administrator();
    denied.is_admin = false;
    assert!(selected_profile(&state, &denied, "secondary").is_err());
    let result = payload(&state, "chatgpt_web", false, "secondary")
        .await
        .expect("closed status");
    assert_eq!(
        result.0["result"]["browser_session"]["browser_profile"],
        "secondary"
    );
    assert_eq!(
        result.0["result"]["browser_session"]["account_identity"],
        "unverified"
    );
    assert_eq!(result.0["result"]["browser_surface"]["active"], false);
    assert_eq!(
        configured_profiles(&state)
            .expect("profiles")
            .into_iter()
            .collect::<Vec<_>>(),
        ["_shared", "secondary"]
    );
}

#[tokio::test]
async fn default_directory_and_login_key_stay_identical_named_directories_are_distinct() {
    let (_temp, state) = captured_state();
    let provider = "chatgpt_web";
    assert_eq!(
        profile_named(&state, provider, "_shared").expect("shared"),
        profile(&state, provider)
    );
    assert_eq!(login_key(provider, ""), key(provider));
    let named = profile_named(&state, provider, "secondary").expect("named path");
    assert_eq!(
        named,
        state
            .home
            .root()
            .join("state/web_model_browser/secondary/chatgpt_web/chrome_profile")
    );
    assert_eq!(
        named.canonicalize().expect("owned directory"),
        state
            .home
            .root()
            .canonicalize()
            .expect("home")
            .join("state/web_model_browser/secondary/chatgpt_web/chrome_profile")
    );
    assert_ne!(named, profile(&state, provider));
    assert_ne!(login_key(provider, "secondary"), key(provider));
    assert!(profile_named(&state, provider, "../_shared").is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn named_directory_refuses_symlink_aliases_at_each_component() {
    for component in [
        "state",
        "state/web_model_browser",
        "state/web_model_browser/secondary",
        "state/web_model_browser/secondary/chatgpt_web",
        "state/web_model_browser/secondary/chatgpt_web/chrome_profile",
    ] {
        let (_temp, state) = captured_state();
        let path = state.home.root().join(component);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("parent");
        let other = state.home.root().join("other-profile");
        if path.exists() {
            std::fs::rename(&path, &other).expect("move owned test directory");
        } else {
            std::fs::create_dir(&other).expect("other owned test directory");
        }
        std::os::unix::fs::symlink(&other, &path).expect("alias control");
        assert!(
            profile_named(&state, "chatgpt_web", "secondary").is_err(),
            "{component}"
        );
    }
}
