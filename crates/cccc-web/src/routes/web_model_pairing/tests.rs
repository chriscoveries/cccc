use super::*;
use axum::response::IntoResponse;
use base64::Engine;
use cccc_contracts::{Actor, ActorRuntime, DaemonRequest};
use cccc_core::{GroupStore, HomeLayout};
use chromiumoxide::{
    Page,
    cdp::browser_protocol::fetch::{
        EnableParams, EventRequestPaused, FulfillRequestParams, HeaderEntry,
    },
};
use futures_util::StreamExt;

// Fetch interception serves synthetic ChatGPT-origin HTML and tool replies.
// No request goes to ChatGPT, no login/profile/provider credits are used.
const HTML: &str = r#"<!doctype html><main><form onsubmit="send(event)"><textarea id="prompt-textarea" style="width:500px;height:120px"></textarea><button type="submit">Send</button></form></main><script>
window.submits=0; window.holdReceipt=true; window.holdCall=false;
async function send(e) {
 e.preventDefault(); const t=document.querySelector('textarea'); const text=t.value; window.submits++;
 const u=document.createElement('article'); u.dataset.messageAuthorRole='user';u.textContent=text;document.querySelector('main').append(u);t.value='';
 if(location.pathname==='/') history.replaceState(null,'',window.provisionalPath || '/c/WEB:00000000-0000-4000-8000-000000000001');
 if(window.holdCall) return;
 const match=text.match(/wm_pair_[a-f0-9]+/);
 if(!match) { (window.business ||= []).push(text); return; }
 const code=match[0]; window.reply=null;
 const r=await (await fetch('/pair-tool?code='+encodeURIComponent(code))).json();
 window.reply=r.result.structuredContent;
 if(!window.holdReceipt) showReceipt();
}
function showReceipt(){ const a=document.createElement('article');a.dataset.messageAuthorRole='assistant';a.textContent=window.reply.receipt;document.querySelector('main').append(a); }
</script>"#;

async fn offline_page(
    state: &AppState,
    group: &str,
    actor: &str,
    profile: &std::path::Path,
    root_url: &str,
) -> (Page, tokio::task::JoinHandle<()>) {
    let key = super::super::web_model_browser::key(group, actor);
    state
        .browser_surfaces
        .open(&key, profile, root_url, 800, 600)
        .await
        .expect("fixture browser");
    intercept_pairing_page(state, &key, actor).await
}

async fn intercept_pairing_page(
    state: &AppState,
    key: &str,
    actor: &str,
) -> (Page, tokio::task::JoinHandle<()>) {
    let page = state
        .browser_surfaces
        .sessions
        .lock()
        .await
        .get(key)
        .expect("session")
        .page
        .clone();
    let mut events = page
        .event_listener::<EventRequestPaused>()
        .await
        .expect("intercept");
    page.execute(EnableParams::default())
        .await
        .expect("intercept all requests");
    let control = page.clone();
    let app = state.clone();
    let session = actor.to_owned();
    let handler = tokio::spawn(async move {
        while let Some(e) = events.next().await {
            let body = if e.request.url.contains("/pair-tool?") {
                let url = reqwest::Url::parse(&e.request.url).expect("fixture URL");
                let code = url
                    .query_pairs()
                    .find(|(key, _)| key == "code")
                    .expect("fixture code")
                    .1
                    .to_string();
                let posted = json!({"code":code});
                let connector = store::load(&app).expect("connector")[0].clone();
                super::super::web_model_connector_session::handle(&app, &connector, &json!({
                    "jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"cccc_pair","arguments":posted,"_meta":{"openai/session":session}}
                })).await.expect("MCP pairing").to_string()
            } else {
                HTML.to_owned()
            };
            let mut response = FulfillRequestParams::new(e.request_id.clone(), 200);
            response.response_headers = Some(vec![HeaderEntry::new(
                "Content-Type",
                if e.request.url.contains("/pair-tool?") {
                    "application/json"
                } else {
                    "text/html"
                },
            )]);
            response.body = Some(
                base64::engine::general_purpose::STANDARD
                    .encode(body)
                    .into(),
            );
            if control.execute(response).await.is_err() {
                break;
            }
        }
    });
    page.goto(format!("https://chatgpt.com/c/{actor}"))
        .await
        .expect("offline ChatGPT origin");
    (page, handler)
}
async fn page_value(page: &Page, script: &str) -> Value {
    page.evaluate(script)
        .await
        .expect("evaluate")
        .into_value()
        .unwrap_or(Value::Null)
}
async fn log_replacement_readiness(page: &Page, phase: &str) {
    // Observe the fixture without marking the composer or changing the page.
    match page
        .evaluate(
            r#"(() => {
                const box = document.querySelector('#prompt-textarea');
                const visible = node => {
                    if (!node || node.closest('[aria-hidden="true"], [inert]')) return false;
                    const rect = node.getBoundingClientRect();
                    const style = getComputedStyle(node);
                    return rect.width > 0 && rect.height > 0 && style.display !== 'none'
                        && style.visibility !== 'hidden' && Number.parseFloat(style.opacity || '1') > 0.01;
                };
                const rect = box?.getBoundingClientRect();
                const composerVisible = visible(box) && rect.width >= 16 && rect.height >= 16
                    && rect.bottom > 0 && rect.right > 0 && rect.top < innerHeight && rect.left < innerWidth;
                const running = [...document.querySelectorAll('button, [role="button"]')]
                    .filter(visible)
                    .some(node => {
                        const label = [node.getAttribute('aria-label'), node.getAttribute('title'),
                            node.getAttribute('data-testid'), node.id, node.innerText || node.textContent]
                            .join(' ').replace(/\s+/g, ' ').trim().toLowerCase();
                        return /\bstop\b|停止|中止|cancel generation|interrupt/.test(label);
                    });
                return {
                    url: location.href,
                    ready_state: document.readyState,
                    composer_present: box !== null,
                    composer_visible: composerVisible,
                    composer_empty: box ? box.value.trim() === '' : null,
                    composer_disabled: box ? box.disabled || box.readOnly : null,
                    running,
                };
            })()"#,
        )
        .await
    {
        Ok(snapshot) => eprintln!(
            "pairing readiness {phase}: {:?}",
            snapshot.into_value::<Value>()
        ),
        Err(error) => eprintln!("pairing readiness {phase}: {error:#}"),
    }
}

async fn wait_page(page: &Page, script: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while page_value(page, script).await != true {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("page condition");
}
async fn show_receipt(page: &Page) {
    // Daemon acceptance precedes delivery of the HTTP response to the page.
    // Wait for the browser to receive it before simulating the assistant reply.
    wait_page(page, "!!window.reply?.receipt").await;
    page_value(page, "showReceipt()").await;
}
fn stored_pair(state: &AppState, group: &str, actor: &str) -> Option<Value> {
    web_model_connectors::pairing_for_actor(&store::load(state).expect("load")[0], group, actor)
}
async fn wait_pair(state: &AppState, group: &str, actor: &str, expected: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(p) = stored_pair(state, group, actor)
                && p["state"] == expected
            {
                return p;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("pair state")
}
async fn connect_actor(state: &AppState, group: &str, actor: &str) -> Value {
    change(
        State(state.clone()),
        Json(json!({"group_id":group,"actor_id":actor,"action":"connect"})),
    )
    .await
    .expect("start connection")
    .0["result"]
        .clone()
}

async fn pairing_daemon(
    home: &HomeLayout,
) -> (
    cccc_client::DaemonClient,
    tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    let daemon_home = home.clone();
    let daemon = tokio::spawn(async move { cccc_daemon::run(daemon_home).await });
    let client = cccc_client::DaemonClient::new(home.clone());
    tokio::time::timeout(Duration::from_secs(5), async {
        while client
            .call(&DaemonRequest {
                v: 1,
                op: "ping".into(),
                args: Map::new(),
            })
            .await
            .is_err()
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("daemon ready");
    (client, daemon)
}

async fn offline_entry() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listen");
    let root_url = format!("http://{}/", listener.local_addr().expect("address"));
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route("/", axum::routing::get(|| async { "offline entry" })),
        )
        .await
        .expect("offline entry");
    });
    (root_url, server)
}

#[tokio::test]
async fn automatic_pairing_verifies_owned_receipts_and_preserves_drafts_routes_and_cancellation() {
    if crate::system_browser_path().is_none() {
        return;
    }
    let _chrome = crate::browser_surface::chrome_test_guard().await;
    let temp = tempfile::tempdir().expect("temp");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    home.initialize().expect("init");
    let groups = GroupStore::new(home.clone()).expect("groups");
    let mut group = groups.create("pairing", "").expect("group");
    for id in ["alpha", "beta"] {
        let mut actor = Actor::new(id);
        actor.runtime = ActorRuntime::WebModel;
        actor.enabled = false;
        cccc_core::actors::add(&mut group, actor).expect("actor");
    }
    groups.save(&group).expect("save");
    let configured = web_model_connectors::configure(&home).expect("connector");
    let (client, daemon) = pairing_daemon(&home).await;
    let shutdown = tokio::sync::broadcast::channel(1).0;
    let (_, _, _, state) = crate::app_with_shutdown(
        home.clone(),
        shutdown.clone(),
        crate::WebMode::Normal,
        None,
        crate::LiveBinding {
            host: "127.0.0.1".into(),
            port: 0,
        },
        "pairing-fixture".into(),
    );
    let (root_url, server) = offline_entry().await;
    let gid = &group.group_id;
    let (a, ah) = offline_page(&state, gid, "alpha", &temp.path().join("alpha"), &root_url).await;
    let (b, bh) = offline_page(&state, gid, "beta", &temp.path().join("beta"), &root_url).await;
    let alpha_key = super::super::web_model_browser::key(gid, "alpha");
    let stale_prompt = crate::browser_surface::pairing_prompt(&format!(
        "wm_pair_{}",
        "0123456789abcdef".repeat(4)
    ));
    page_value(
        &a,
        &format!(
            "document.querySelector('textarea').value={}",
            json!(stale_prompt.as_str())
        ),
    )
    .await;
    state
        .browser_surfaces
        .pairing_page(&alpha_key)
        .await
        .expect("an earlier pairing prompt is not a human draft");
    assert_eq!(
        page_value(&a, "document.querySelector('textarea').value").await,
        ""
    );
    assert_eq!(page_value(&a, "window.submits").await, 0);
    let stale_with_note = format!("{stale_prompt} Keep my note");
    page_value(
        &a,
        &format!(
            "document.querySelector('textarea').value={}",
            json!(stale_with_note.as_str())
        ),
    )
    .await;
    let error = match state.browser_surfaces.pairing_page(&alpha_key).await {
        Ok(_) => panic!("a stale prompt with appended text is not ours"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("pairing_composer_occupied"));
    assert_eq!(
        page_value(&a, "document.querySelector('textarea').value").await,
        stale_with_note
    );
    page_value(&a, "document.querySelector('textarea').value=''").await;
    page_value(
        &a,
        "document.querySelector('textarea').value='Keep my private draft'",
    )
    .await;
    let result = change(
        State(state.clone()),
        Json(json!({"group_id":gid,"actor_id":"alpha","action":"connect"})),
    )
    .await;
    assert!(
        result
            .expect_err("must protect draft")
            .to_string()
            .contains("pairing_composer_occupied")
    );
    assert!(stored_pair(&state, gid, "alpha").is_none());
    assert_eq!(
        page_value(&a, "document.querySelector('textarea').value").await,
        "Keep my private draft"
    );
    assert_eq!(page_value(&a, "window.submits").await, 0);
    page_value(&a, "document.querySelector('textarea').value=''").await;
    page_value(&a, "document.querySelector('form').insertAdjacentHTML('beforeend','<div data-testid=attachment>Unsent file</div>')").await;
    assert!(
        change(
            State(state.clone()),
            Json(json!({"group_id":gid,"actor_id":"alpha","action":"connect"}))
        )
        .await
        .expect_err("pending attachment")
        .to_string()
        .contains("pairing_composer_occupied")
    );
    // A pending attachment is checked before the stale-prompt clear. The
    // attachment and the text must both survive the refusal.
    page_value(
        &a,
        &format!(
            "document.querySelector('textarea').value={}",
            json!(stale_prompt.as_str())
        ),
    )
    .await;
    let error = match state.browser_surfaces.pairing_page(&alpha_key).await {
        Ok(_) => panic!("a pending attachment is not a stale draft"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("pairing_composer_occupied"));
    assert_eq!(
        page_value(&a, "document.querySelector('textarea').value").await,
        stale_prompt
    );
    page_value(
        &a,
        "document.querySelector('[data-testid=attachment]').remove()",
    )
    .await;
    page_value(&a, "document.querySelector('textarea').value=''").await;
    // A connector icon and the upload button are not unsent user attachments.
    page_value(&a, "document.querySelector('form').insertAdjacentHTML('beforeend','<img alt=CCCC width=24 height=24 src=/connector-icon.png><button type=button data-testid=attachment-button>Add files</button>')").await;
    // An interrupted attempt's unsent setup message is CCCC's own, and a retry
    // removes it without sending it. With anything added it may be a human's.
    let stale = crate::browser_surface::pairing_prompt(&format!(
        "wm_pair_{}",
        "0123456789abcdef".repeat(4)
    ));
    let amended = format!("{stale} Also check the build.");
    page_value(
        &a,
        &format!(
            "document.querySelector('textarea').value={}",
            json!(amended)
        ),
    )
    .await;
    assert!(
        change(
            State(state.clone()),
            Json(json!({"group_id":gid,"actor_id":"alpha","action":"connect"}))
        )
        .await
        .expect_err("added text may be a human's")
        .to_string()
        .contains("pairing_composer_occupied")
    );
    assert_eq!(
        page_value(&a, "document.querySelector('textarea').value").await,
        json!(amended)
    );
    assert!(stored_pair(&state, gid, "alpha").is_none());
    // The page's own focus handling can change the box after the first read, so the
    // last checks come after focusing: text rewritten or a file added then is kept.
    // A one-time focus() replacement stands for such a handler, as a page whose
    // window lacks focus may not dispatch the event; its removal shows the clear ran.
    for (on_focus, kept) in [
        (
            "this.value = 'Typed while connecting'",
            json!("Typed while connecting"),
        ),
        (
            "this.form.insertAdjacentHTML('beforeend', '<div data-testid=attachment>Unsent file</div>')",
            json!(stale),
        ),
    ] {
        page_value(
            &a,
            &format!(
                "() => {{ const t = document.querySelector('textarea'); t.value = {}; \
                 t.focus = function () {{ delete this.focus; this.focus(); {on_focus}; }}; }}",
                json!(stale)
            ),
        )
        .await;
        assert!(
            change(
                State(state.clone()),
                Json(json!({"group_id":gid,"actor_id":"alpha","action":"connect"}))
            )
            .await
            .expect_err("changed while focusing")
            .to_string()
            .contains("pairing_composer_occupied")
        );
        assert_eq!(
            page_value(
                &a,
                "Object.hasOwn(document.querySelector('textarea'), 'focus')"
            )
            .await,
            false
        );
        assert_eq!(
            page_value(&a, "document.querySelector('textarea').value").await,
            kept
        );
        assert!(stored_pair(&state, gid, "alpha").is_none());
    }
    page_value(
        &a,
        "document.querySelector('[data-testid=attachment]').remove()",
    )
    .await;
    page_value(
        &a,
        &format!("document.querySelector('textarea').value={}", json!(stale)),
    )
    .await;
    let started = connect_actor(&state, gid, "alpha").await;
    assert!(
        started["code"].is_null(),
        "no copyable code in the UI response"
    );
    assert!(
        change(
            State(state.clone()),
            Json(json!({"group_id":gid,"actor_id":"alpha","action":"connect"}))
        )
        .await
        .is_err(),
        "duplicate click cannot send twice"
    );
    wait_page(&a, "!!window.reply?.receipt").await;
    let accepted = wait_pair(&state, gid, "alpha", "awaiting_confirmation").await;
    assert!(
        web_model_connectors::binding_for_session(
            &store::load(&state).expect("store")[0],
            accepted["session_key"].as_str().expect("session")
        )
        .is_none(),
        "MCP accept alone grants nothing"
    );
    page_value(&b,&format!("document.body.insertAdjacentHTML('beforeend', '<article data-message-author-role=\"assistant\">'+{}+'</article>')",accepted["receipt"])).await;
    // User-role echo in A must also not count as an assistant reply.
    page_value(
        &a,
        &format!(
            "document.querySelector('textarea').value={}",
            accepted["receipt"]
        ),
    )
    .await;
    let target = state
        .browser_surfaces
        .pairing_page(&super::super::web_model_browser::key(gid, "beta"))
        .await
        .expect("B remains independently ready");
    assert!(
        target
            .receipt_url(
                &state.browser_surfaces,
                &super::super::web_model_browser::key(gid, "beta"),
                "not sent here",
                accepted["receipt"].as_str()
            )
            .await
            .expect("inspect B")
            .is_none()
    );
    page_value(
        &a,
        "document.querySelector('textarea').value='';showReceipt()",
    )
    .await;
    wait_pair(&state, gid, "alpha", "bound").await;
    assert_eq!(page_value(&a, "window.submits").await, 1);

    // B can pair independently through the same connector, including a new chat.
    page_value(
        &b,
        "history.replaceState(null,'','/g/g-p-fixture/project');window.holdReceipt=false;window.provisionalPath='/g/g-p-fixture/c/local-chatgpt%3A00000000-0000-4000-8000-000000000001'",
    )
    .await;
    let beta_key = super::super::web_model_browser::key(gid, "beta");
    let new_chat = state
        .browser_surfaces
        .pairing_page(&beta_key)
        .await
        .expect("new chat");
    // Both provisional URL families must wait for a stable server ID.
    for path in [
        "/c/WEB:00000000-0000-4000-8000-000000000001",
        "/c/local-chatgpt:00000000-0000-4000-8000-000000000001",
        "/c/local-chatgpt%3A00000000-0000-4000-8000-000000000001",
        "/g/g-p-fixture/c/local-chatgpt%3A00000000-0000-4000-8000-000000000001",
    ] {
        page_value(
            &b,
            &format!("history.replaceState(null,'',{})", json!(path)),
        )
        .await;
        assert!(
            new_chat
                .receipt_url(&state.browser_surfaces, &beta_key, "not sent yet", None)
                .await
                .expect("provisional new chat is not a target change")
                .is_none()
        );
        assert!(
            state
                .browser_surfaces
                .pairing_page(&beta_key)
                .await
                .is_err(),
            "a provisional URL cannot start a separate pairing"
        );
    }
    page_value(&b, "history.replaceState(null,'','/g/g-p-fixture/project')").await;
    connect_actor(&state, gid, "beta").await;
    wait_page(&b, "!!window.reply?.receipt").await;
    let pending = wait_pair(&state, gid, "beta", "awaiting_confirmation").await;
    let prompt = page_value(
        &b,
        "document.querySelector('[data-message-author-role=user]').innerText",
    )
    .await;
    assert!(
        new_chat
            .receipt_url(
                &state.browser_surfaces,
                &beta_key,
                prompt.as_str().expect("setup text"),
                pending["receipt"].as_str()
            )
            .await
            .expect("receipt on a provisional URL waits")
            .is_none()
    );
    assert!(
        web_model_connectors::binding_for_session(
            &store::load(&state).expect("store")[0],
            pending["session_key"].as_str().expect("session")
        )
        .is_none(),
        "a visible receipt cannot authorize a provisional conversation"
    );
    page_value(&b, "history.replaceState(null,'','/c/new-fixture')").await;
    wait_pair(&state, gid, "beta", "bound").await;
    assert_eq!(page_value(&b, "window.submits").await, 1);
    let before_move = state
        .browser_surfaces
        .pairing_page(&beta_key)
        .await
        .expect("saved chat");
    page_value(
        &b,
        "history.replaceState(null,'','/g/g-p-fixture/c/new-fixture')",
    )
    .await;
    before_move
        .check_before_send(&state.browser_surfaces, &beta_key)
        .await
        .expect("same owned conversation");
    assert_eq!(
        before_move
            .receipt_url(
                &state.browser_surfaces,
                &beta_key,
                prompt.as_str().expect("setup text"),
                pending["receipt"].as_str()
            )
            .await
            .expect("Project move")
            .as_deref(),
        Some("https://chatgpt.com/g/g-p-fixture/c/new-fixture")
    );
    // Moving a bound chat into a Project must preserve its exact-batch receipt
    // without navigating away from the current page or sending a second time.
    let marker = "[cccc] Browser batch project-fixture events=fixture actor=beta";
    page_value(&b, &format!("history.replaceState(null,'','/g/g-p-fixture/c/new-fixture');const m=document.createElement('article');m.dataset.messageAuthorRole='user';m.textContent={};document.querySelector('main').append(m)",json!(marker))).await;
    state
        .browser_surfaces
        .align_chatgpt_conversation_target(
            &beta_key,
            "https://chatgpt.com/c/new-fixture",
            Duration::from_millis(50),
        )
        .await
        .expect("Project route retains the bound conversation");
    assert_eq!(
        page_value(&b, "location.pathname").await,
        "/g/g-p-fixture/c/new-fixture"
    );
    assert!(
        state
            .browser_surfaces
            .inspect_delivery_receipt(&beta_key, "https://chatgpt.com/c/new-fixture", marker)
            .await
            .expect("same conversation receipt")
            .is_some()
    );
    assert!(
        state
            .browser_surfaces
            .inspect_delivery_receipt(&beta_key, "https://chatgpt.com/c/other-fixture", marker)
            .await
            .expect("different conversation is not a receipt")
            .is_none()
    );
    assert_eq!(page_value(&b, "window.submits").await, 1);
    page_value(&b, "history.replaceState(null,'','/c/new-fixture')").await;
    let stable_beta = state
        .browser_surfaces
        .pairing_page(&beta_key)
        .await
        .expect("stable B");
    page_value(
        &b,
        "history.replaceState(null,'','/c/WEB:00000000-0000-4000-8000-000000000001')",
    )
    .await;
    assert_eq!(
        stable_beta
            .receipt_url(&state.browser_surfaces, &beta_key, "", None)
            .await
            .expect_err("existing conversation cannot switch to provisional chat")
            .to_string(),
        "pairing_target_changed"
    );
    page_value(&b, "history.replaceState(null,'','/c/new-fixture')").await;
    let connector = store::load(&state).expect("store")[0].clone();
    assert_eq!(
        connector["bindings"].as_object().expect("bindings").len(),
        2
    );
    let before = connector["bindings"].clone();

    // Cancel while receipt is held. A late receipt cannot replace the old binding.
    page_value(&a, "window.reply=null;window.holdReceipt=true").await;
    let cancelled = connect_actor(&state, gid, "alpha").await;
    wait_page(&a, "!!window.reply?.receipt").await;
    assert_eq!(
        page_value(&a, "window.submits").await,
        2,
        "a new pairing code must be sent even when setup messages share a prefix"
    );
    let _ = change(State(state.clone()),Json(json!({"group_id":gid,"actor_id":"alpha","action":"cancel","pairing_id":cancelled["pairing_id"]}))).await.expect("cancel");
    show_receipt(&a).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while is_connecting(&state, cancelled["pairing_id"].as_str().expect("id")) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("cancel task ends");
    assert_eq!(
        stored_pair(&state, gid, "alpha").expect("cancel retained")["state"],
        "cancelled"
    );
    assert_eq!(store::load(&state).expect("store")[0]["bindings"], before);

    // A changed conversation rejects receipt completion and retains both routes.
    page_value(&a, "window.reply=null").await;
    connect_actor(&state, gid, "alpha").await;
    wait_page(&a, "!!window.reply?.receipt").await;
    page_value(
        &a,
        "history.replaceState(null,'','/c/wrong-conversation');showReceipt()",
    )
    .await;
    let failed = wait_pair(&state, gid, "alpha", "failed").await;
    assert_eq!(failed["error_code"], "pairing_target_changed");
    assert_eq!(store::load(&state).expect("store")[0]["bindings"], before);

    // A cleared composer with no user-message echo is ambiguous, never retried.
    page_value(&b, "window.uncertainSends=0;document.querySelector('form').onsubmit=e=>{e.preventDefault();window.uncertainSends++;document.querySelector('textarea').value='';}").await;
    connect_actor(&state, gid, "beta").await;
    let uncertain = wait_pair(&state, gid, "beta", "failed").await;
    assert_eq!(uncertain["error_code"], "pairing_submission_uncertain");
    assert_eq!(page_value(&b, "window.uncertainSends").await, 1);
    assert_eq!(store::load(&state).expect("store")[0]["bindings"], before);

    // An interrupted old attempt is observational only after a Web restart.
    let old = web_model_connectors::begin_pairing(
        &home,
        configured["connector"]["connector_id"]
            .as_str()
            .expect("id"),
        gid,
        "alpha",
        &group.actors[0].generation,
        false,
    )
    .expect("orphan");
    assert!(!is_connecting(
        &state,
        old["pairing_id"].as_str().expect("id")
    ));
    assert_eq!(page_value(&a, "window.submits").await, 3);

    // Normal startup connects both Actors with no pairing POST, and leaves the
    // real message queued until both the MCP call and owned receipt are proven.
    for actor in ["alpha", "beta"] {
        web_model_connectors::retire_actor(&home, gid, actor).expect("reset fixture routes");
    }
    a.goto("https://chatgpt.com/c/alpha")
        .await
        .expect("fresh page");
    page_value(&a, "history.replaceState(null,'','/')").await;
    b.goto("https://chatgpt.com/c/beta")
        .await
        .expect("fresh page");
    let invoke = |op: &str, args: Value| {
        let app = state.clone();
        let op = op.to_owned();
        async move {
            call(&app, &op, args.as_object().expect("args").clone())
                .await
                .expect("daemon operation")
        }
    };
    for actor in ["alpha", "beta"] {
        invoke(
            "actor_start",
            json!({"group_id":gid,"actor_id":actor,"by":"user"}),
        )
        .await;
    }
    let message = invoke(
        "message_send",
        json!({"group_id":gid,"by":"user","to":["alpha"],"text":"Unique queued task 84973","message_mode":"send"}),
    )
    .await;
    assert!(!message.is_null());
    page_value(
        &a,
        "document.querySelector('textarea').value='preserve before automatic connection'",
    )
    .await;
    super::super::web_model_supervisor::ensure_running_actor(&state, Some(gid)).await;
    assert!(
        stored_pair(&state, gid, "alpha").is_none(),
        "draft defers initial send"
    );
    assert_eq!(page_value(&a, "window.submits").await, 0);
    wait_pair(&state, gid, "beta", "awaiting_confirmation").await;
    page_value(&a, "document.querySelector('textarea').value=''").await;
    super::super::web_model_supervisor::ensure_running_actor(&state, Some(gid)).await;
    wait_pair(&state, gid, "alpha", "awaiting_confirmation").await;
    assert_eq!(page_value(&a, "window.submits").await, 1);
    assert_eq!(page_value(&a, "(window.business||[]).length").await, 0);
    let pending = cccc_core::integration_state::group_get(&groups, gid, "runtime_states")
        .expect("runtime state");
    assert_ne!(pending["alpha"]["status"], "working");
    show_receipt(&a).await;
    assert!(store::for_actor(&state, gid, "alpha").is_none());
    assert_eq!(page_value(&a, "(window.business||[]).length").await, 0);
    page_value(&a, "history.replaceState(null,'','/c/alpha')").await;
    show_receipt(&b).await;
    wait_pair(&state, gid, "alpha", "bound").await;
    wait_pair(&state, gid, "beta", "bound").await;
    wait_page(
        &a,
        "(window.business||[]).some(t=>t.includes('Unique queued task 84973'))",
    )
    .await;
    assert_eq!(
        page_value(
            &a,
            "window.business.filter(t=>t.includes('Unique queued task 84973')).length"
        )
        .await,
        1
    );
    assert_eq!(
        page_value(
            &b,
            "(window.business||[]).some(t=>t.includes('Unique queued task 84973'))"
        )
        .await,
        false
    );
    let bound = store::for_actor(&state, gid, "beta").expect("B bound");
    // A normal restart uses the established binding, without another handshake.
    invoke(
        "actor_stop",
        json!({"group_id":gid,"actor_id":"beta","by":"user"}),
    )
    .await;
    invoke(
        "actor_start",
        json!({"group_id":gid,"actor_id":"beta","by":"user"}),
    )
    .await;
    super::super::web_model_supervisor::ensure_running_actor(&state, Some(gid)).await;
    assert_eq!(
        store::for_actor(&state, gid, "beta").expect("B still bound")["revision"],
        bound["revision"]
    );
    assert_eq!(page_value(&b, "window.submits").await, 1);

    // Rapid stop/start cannot revive an accepted but unverified handshake.
    invoke(
        "actor_stop",
        json!({"group_id":gid,"actor_id":"beta","by":"user"}),
    )
    .await;
    web_model_connectors::retire_actor(&home, gid, "beta").expect("reset B route");
    b.goto("https://chatgpt.com/c/beta").await.expect("fresh B");
    invoke(
        "actor_start",
        json!({"group_id":gid,"actor_id":"beta","by":"user"}),
    )
    .await;
    super::super::web_model_supervisor::ensure_running_actor(&state, Some(gid)).await;
    wait_pair(&state, gid, "beta", "awaiting_confirmation").await;
    invoke(
        "actor_stop",
        json!({"group_id":gid,"actor_id":"beta","by":"user"}),
    )
    .await;
    invoke(
        "actor_start",
        json!({"group_id":gid,"actor_id":"beta","by":"user"}),
    )
    .await;
    show_receipt(&b).await;
    let interrupted = wait_pair(&state, gid, "beta", "failed").await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while is_connecting(&state, interrupted["pairing_id"].as_str().expect("id")) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("old task exits");
    super::super::web_model_supervisor::ensure_running_actor(&state, Some(gid)).await;
    assert_eq!(
        page_value(&b, "window.submits").await,
        1,
        "interrupted send not retried"
    );
    assert!(store::for_actor(&state, gid, "beta").is_none());
    // Explicit retry is available while running. Cancelling it also fences timers.
    page_value(&b, "window.reply=null").await;
    let retry = connect_actor(&state, gid, "beta").await;
    wait_pair(&state, gid, "beta", "awaiting_confirmation").await;
    let _ = change(State(state.clone()),Json(json!({"group_id":gid,"actor_id":"beta","action":"cancel","pairing_id":retry["pairing_id"]}))).await.expect("cancel running attempt");
    show_receipt(&b).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while is_connecting(&state, retry["pairing_id"].as_str().expect("id")) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("cancel task exits");
    super::super::web_model_supervisor::ensure_running_actor(&state, Some(gid)).await;
    assert_eq!(page_value(&b, "window.submits").await, 2);
    assert_eq!(
        stored_pair(&state, gid, "beta").expect("cancel fence")["state"],
        "cancelled"
    );
    assert!(store::for_actor(&state, gid, "beta").is_none());
    page_value(&b, "window.holdReceipt=false").await;
    connect_actor(&state, gid, "beta").await;
    wait_pair(&state, gid, "beta", "bound").await;

    // Group pause/resume also invalidates an in-progress handshake, even when
    // the Actor remains enabled. Neither resume nor an expired orphan resends.
    invoke(
        "actor_stop",
        json!({"group_id":gid,"actor_id":"beta","by":"user"}),
    )
    .await;
    web_model_connectors::retire_actor(&home, gid, "beta").expect("reset B route");
    b.goto("https://chatgpt.com/c/beta").await.expect("fresh B");
    invoke(
        "actor_start",
        json!({"group_id":gid,"actor_id":"beta","by":"user"}),
    )
    .await;
    super::super::web_model_supervisor::ensure_running_actor(&state, Some(gid)).await;
    wait_pair(&state, gid, "beta", "awaiting_confirmation").await;
    invoke(
        "group_set_state",
        json!({"group_id":gid,"state":"paused","by":"user"}),
    )
    .await;
    invoke(
        "group_set_state",
        json!({"group_id":gid,"state":"active","by":"user"}),
    )
    .await;
    show_receipt(&b).await;
    let paused = wait_pair(&state, gid, "beta", "failed").await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while is_connecting(&state, paused["pairing_id"].as_str().expect("id")) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("paused task exits");
    super::super::web_model_supervisor::ensure_running_actor(&state, Some(gid)).await;
    assert!(store::for_actor(&state, gid, "beta").is_none());
    let cid = configured["connector"]["connector_id"]
        .as_str()
        .expect("id");
    let orphan = web_model_connectors::begin_pairing(
        &home,
        cid,
        gid,
        "beta",
        &group.actors[1].generation,
        true,
    )
    .expect("orphan after restart");
    assert!(!is_connecting(
        &state,
        orphan["pairing_id"].as_str().expect("id")
    ));
    super::super::web_model_supervisor::ensure_running_actor(&state, Some(gid)).await;
    store::update_connector(&state, cid, |c| {
        for p in c["pairings"].as_object_mut().expect("pairs").values_mut() {
            if p["actor_id"] == "beta" {
                p["expires_at_ms"] = json!(0);
            }
        }
    })
    .expect("expire orphan");
    super::super::web_model_supervisor::ensure_running_actor(&state, Some(gid)).await;
    assert_eq!(
        page_value(&b, "window.submits").await,
        1,
        "no resumed or expired replay"
    );
    // A browser read failure does not prove that the user changed conversations.
    // It still fails closed and must not change another Actor's binding.
    let alpha_binding = store::for_actor(&state, gid, "alpha");
    page_value(&b, "window.reply=null").await;
    connect_actor(&state, gid, "beta").await;
    wait_pair(&state, gid, "beta", "awaiting_confirmation").await;
    page_value(&b, r#"() => {
        const query = document.querySelectorAll.bind(document);
        document.querySelectorAll = s => {
            if (s.includes('data-message-author-role="user"') && s.includes('data-message-author-role="assistant"')) throw new Error('fixture inspection failure');
            return query(s);
        };
    }"#).await;
    let failed = wait_pair(&state, gid, "beta", "failed").await;
    assert_eq!(failed["error_code"], "pairing_interrupted");
    assert!(store::for_actor(&state, gid, "beta").is_none());
    assert_eq!(store::for_actor(&state, gid, "alpha"), alpha_binding);
    // The same stale-prompt clear also supports a contenteditable composer.
    let (gamma, gh) =
        offline_page(&state, gid, "gamma", &temp.path().join("gamma"), &root_url).await;
    let gamma_key = super::super::web_model_browser::key(gid, "gamma");
    page_value(
        &gamma,
        &format!(
            "() => {{ const t=document.querySelector('textarea'); t.outerHTML='<div id=prompt-textarea contenteditable=true role=textbox style=\"width:400px;height:80px\"></div>'; document.querySelector('#prompt-textarea').textContent={}; }}",
            json!(stale_prompt.as_str())
        ),
    )
    .await;
    state
        .browser_surfaces
        .pairing_page(&gamma_key)
        .await
        .expect("contenteditable stale prompt is cleared");
    assert_eq!(
        page_value(
            &gamma,
            "document.querySelector('#prompt-textarea').innerText"
        )
        .await,
        ""
    );
    gh.abort();
    state
        .browser_surfaces
        .shutdown_all()
        .await
        .expect("close fixtures");
    ah.abort();
    bh.abort();
    server.abort();
    let _ = shutdown.send(());
    client
        .call(&DaemonRequest {
            v: 1,
            op: "shutdown".into(),
            args: Map::new(),
        })
        .await
        .expect("shutdown");
    daemon.await.expect("task").expect("daemon");
}

async fn finish_pairing(state: &AppState, group: &str, actor: &str, page: &Page) {
    let pending = wait_pair(state, group, actor, "awaiting_confirmation").await;
    show_receipt(page).await;
    wait_pair(state, group, actor, "bound").await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while is_connecting(state, pending["pairing_id"].as_str().expect("id")) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("pairing task released browser work");
}

#[tokio::test]
async fn explicit_pairing_repairs_a_stale_binding_and_preserves_a_project_chat_bootstrap() {
    use super::super::{
        web_model_browser, web_model_delivery, web_model_delivery_state, web_model_shared_browser,
    };
    use cccc_contracts::GroupState;

    if crate::system_browser_path().is_none() {
        return;
    }
    let _chrome = crate::browser_surface::chrome_test_guard().await;
    let temp = tempfile::tempdir().expect("fixture");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    home.initialize().expect("init");
    let groups = GroupStore::new(home.clone()).expect("groups");
    let mut group = groups.create("repair", "").expect("group");
    let mut actor = Actor::new("repair-fixture");
    actor.runtime = ActorRuntime::WebModel;
    actor.enabled = false;
    actor
        .env
        .insert("CCCC_WEB_MODEL_DELIVERY_MODE".into(), "browser".into());
    let actor = cccc_core::actors::add(&mut group, actor).expect("actor");
    group.running = true;
    group.state = GroupState::Active;
    groups.save(&group).expect("save");
    web_model_connectors::configure(&home).expect("connector");

    let (client, daemon) = pairing_daemon(&home).await;
    let shutdown = tokio::sync::broadcast::channel(1).0;
    let (_, _, surfaces, state) = crate::app_with_shutdown(
        home,
        shutdown.clone(),
        crate::WebMode::Normal,
        None,
        crate::LiveBinding {
            host: "127.0.0.1".into(),
            port: 0,
        },
        "repair-fixture".into(),
    );
    let (entry, server) = offline_entry().await;
    let group_id = &group.group_id;
    let actor_id = actor.id.as_str();
    let key = web_model_browser::key(group_id, actor_id);
    let profile = web_model_shared_browser::profile(&state, "chatgpt_web");
    surfaces
        .open_shared_actor_fixture(
            &key,
            &profile,
            &entry,
            &crate::browser_surface::actor_identity(&actor),
        )
        .await
        .expect("owned browser page");
    let (page, intercept) = intercept_pairing_page(&state, &key, actor_id).await;
    let original_url = "https://chatgpt.com/c/repair-fixture";
    let project_url = "https://chatgpt.com/g/g-p-fixture/c/repair-fixture";
    let replacement_url = "https://chatgpt.com/g/g-p-fixture/c/replacement-fixture";
    let binding = || store::for_actor(&state, group_id, actor_id).expect("binding");
    let target = || web_model_delivery_state::target(&state, group_id, actor_id).expect("target");
    let invoke = |op: &str, extra: Value| {
        let state = state.clone();
        let op = op.to_owned();
        let mut args = extra.as_object().expect("args").clone();
        args.insert("group_id".into(), json!(group_id));
        async move { call(&state, &op, args).await.expect("daemon operation") }
    };
    let lifecycle = |op: &str| invoke(op, json!({"by":"user","actor_id":actor_id}));
    let send = |text: &str| {
        invoke(
            "message_send",
            json!({"by":"user","to":[actor_id],"text":text,"message_mode":"send"}),
        )
    };
    let replacement = |url: &str| json!({"group_id":group_id,"actor_id":actor_id,"action":"connect","replace":true,"conversation_url":url});

    connect_actor(&state, group_id, actor_id).await;
    finish_pairing(&state, group_id, actor_id, &page).await;
    let before_move = binding();
    assert_eq!(before_move["url"], original_url);
    lifecycle("actor_start").await;
    send("Message before the Project move").await;
    assert!(matches!(
        web_model_delivery::deliver_pending(&state, group_id, actor_id)
            .await
            .expect("seed chat"),
        web_model_delivery::DeliveryOutcome::Submitted
    ));
    assert_eq!(target()["bootstrap_seed_conversation_url"], original_url);
    lifecycle("actor_stop").await;

    page.goto(project_url)
        .await
        .expect("chat moved into a Project");
    let _ = change(State(state.clone()), Json(replacement(project_url)))
        .await
        .expect("select the moved chat");
    finish_pairing(&state, group_id, actor_id, &page).await;
    let moved = binding();
    assert_eq!(moved["url"], project_url);
    assert_ne!(moved["revision"], before_move["revision"]);
    assert_eq!(target()["bootstrap_seed_conversation_url"], original_url);
    lifecycle("actor_start").await;
    send("First message after moving the chat").await;
    assert!(matches!(
        web_model_delivery::deliver_pending(&state, group_id, actor_id)
            .await
            .expect("deliver to moved chat"),
        web_model_delivery::DeliveryOutcome::Submitted
    ));
    let prompts = page_value(&page, "window.business").await;
    let prompt = prompts[0].as_str().expect("next prompt");
    assert_eq!(prompts.as_array().expect("prompts").len(), 1);
    assert!(prompt.contains("First message after moving the chat"));
    assert!(!prompt.contains("Session bootstrap"));
    assert!(!prompt.contains("Message before the Project move"));
    let before_repair = binding();

    // Drive the daemon's real claim and completion APIs to leave a settled
    // ambiguous delivery with the same persisted evidence as a browser send.
    send("Earlier ambiguous message").await;
    let wait = invoke(
        "runtime_wait_next_turn",
        json!({"actor_id":actor_id,"transport":"web_model_browser"}),
    )
    .await;
    assert_eq!(wait["status"], "work_available");
    let turn = &wait["turn"];
    let delivery = "browser-ambiguous-fixture";
    let mut attempt = json!({"actor_id":actor_id,"by":actor_id,"turn_id":turn["turn_id"],
        "event_ids":turn["event_ids"],"delivery_id":delivery});
    attempt["browser_delivery"] = json!({"state":"ambiguous","detail":"send outcome unknown"});
    invoke("web_model_browser_delivery_record", attempt.clone()).await;
    attempt["status"] = json!("done");
    invoke("runtime_complete_turn", attempt).await;
    web_model_delivery_state::update_target(
        &state,
        group_id,
        actor_id,
        json!({
            "last_delivery_status":"submission_ambiguous","last_delivery_id":delivery,
            "last_delivery_turn_id":turn["turn_id"],"last_delivery_event_ids":turn["event_ids"]
        }),
    )
    .expect("ambiguous target");
    lifecycle("actor_stop").await;
    let ambiguous = target();
    page_value(&page, "document.querySelector('textarea').hidden=true").await;
    let refused = change(
        State(state.clone()),
        Json(json!({"group_id":group_id,"actor_id":actor_id,"action":"remove"})),
    )
    .await
    .expect_err("unreachable old composer prevents removal");
    assert_eq!(refused.code(), "delivery_unresolved");
    page_value(&page, "document.querySelector('textarea').hidden=false").await;
    lifecycle("actor_start").await;
    let refused = change(
        State(state.clone()),
        Json(json!({"group_id":group_id,"actor_id":actor_id,"action":"connect"})),
    )
    .await
    .expect_err("ordinary connect still sees the binding");
    assert_eq!(refused.code(), "already_paired");
    lifecycle("actor_stop").await;
    let mut repair = replacement(replacement_url);
    repair["expected_binding_revision"] = moved["revision"].clone();
    repair["abandon_delivery_id"] = json!(delivery);
    for (field, draft) in [
        ("expected_binding_revision", ""),
        ("abandon_delivery_id", "Unsent draft"),
    ] {
        page_value(
            &page,
            &format!("document.querySelector('textarea').value={}", json!(draft)),
        )
        .await;
        let mut stale = repair.clone();
        stale[field] = json!("stale");
        let refused = change(State(state.clone()), Json(stale))
            .await
            .expect_err("stale repair");
        assert_eq!(refused.code(), "delivery_unresolved");
        assert_eq!(page_value(&page, "location.href").await, project_url);
        assert_eq!(
            page_value(&page, "document.querySelector('textarea').value").await,
            draft
        );
        assert_eq!(binding(), before_repair);
        assert_eq!(target(), ambiguous);
    }
    page_value(&page, "document.querySelector('textarea').value=''").await;

    // The composer changes after the preparation probe, as the final
    // document guard selects the current composer for navigation.
    page_value(
        &page,
        r#"() => {
            const box = document.querySelector('textarea');
            const set = box.setAttribute.bind(box);
            let probes = 0;
            box.setAttribute = (name, value) => {
                if (name === 'data-cccc-web-model-composer' && ++probes === 2) {
                    box.value = 'Draft just before navigation';
                    delete box.setAttribute;
                }
                set(name, value);
            };
        }"#,
    )
    .await;
    let refused = change(State(state.clone()), Json(repair.clone()))
        .await
        .expect_err("draft at navigation time blocks replacement");
    assert_eq!(refused.code(), "pairing_composer_occupied");
    let body = axum::body::to_bytes(refused.into_response().into_body(), 4096)
        .await
        .expect("refusal body");
    let error: Value = serde_json::from_slice(&body).expect("refusal JSON");
    let details = &error["error"]["details"];
    assert_eq!(details["phase"], "navigation_guard");
    assert_eq!(details["observed_url"], project_url);
    assert_eq!(details["reason"], "composer_occupied");
    let surface = surfaces.info(&key).await;
    let cached = &surface["metadata"]["prompt_readiness"];
    assert_eq!(cached["ready"], true);
    assert!(cached.get("ready_state").is_none());
    assert!(cached.get("composer_present").is_none());
    assert_eq!(page_value(&page, "location.href").await, project_url);
    assert_eq!(
        page_value(&page, "document.querySelector('textarea').value").await,
        "Draft just before navigation"
    );
    assert_eq!(binding(), before_repair);
    assert_eq!(target(), ambiguous);
    page_value(&page, "document.querySelector('textarea').value=''").await;

    log_replacement_readiness(&page, "before replacement retry").await;
    let started = std::time::Instant::now();
    let result = change(State(state.clone()), Json(repair)).await;
    let elapsed_ms = started.elapsed().as_millis();
    if result
        .as_ref()
        .is_err_and(|error| error.code() == "pairing_browser_not_ready")
    {
        let phase = format!("after replacement retry (elapsed_ms={elapsed_ms})");
        if tokio::time::timeout(
            Duration::from_secs(2),
            log_replacement_readiness(&page, &phase),
        )
        .await
        .is_err()
        {
            eprintln!("pairing readiness {phase}: snapshot timed out");
        }
    }
    let _ = result.expect("one replacement request");
    assert_eq!(
        binding(),
        before_repair,
        "old route survives until confirmation"
    );
    finish_pairing(&state, group_id, actor_id, &page).await;
    let repaired = binding();
    assert_eq!(repaired["url"], replacement_url);
    assert!(web_model_connectors::abandoned_delivery_matches(
        &repaired, &ambiguous
    ));

    page.goto(replacement_url)
        .await
        .expect("reopen repaired chat");
    page_value(&page, "document.querySelector('main').insertAdjacentHTML('beforeend','<article data-message-author-role=assistant>Existing conversation</article>')").await;
    lifecycle("actor_start").await;
    send("Only the later queued message").await;
    assert!(matches!(
        web_model_delivery::deliver_pending(&state, group_id, actor_id)
            .await
            .expect("deliver after repair"),
        web_model_delivery::DeliveryOutcome::Submitted
    ));
    let prompts = page_value(&page, "window.business").await;
    let prompt = prompts[0].as_str().expect("repaired prompt");
    assert_eq!(prompts.as_array().expect("prompts").len(), 1);
    assert!(prompt.contains("Only the later queued message"));
    assert!(
        prompt.contains("Session bootstrap"),
        "new chat needs the seed"
    );
    assert!(!prompt.contains("Earlier ambiguous message"));
    assert_eq!(target()["last_delivery_status"], "submitted");
    assert_ne!(target()["last_delivery_id"], delivery);

    surfaces.shutdown_all().await.expect("close fixture");
    intercept.abort();
    server.abort();
    let _ = shutdown.send(());
    client
        .call(&DaemonRequest {
            v: 1,
            op: "shutdown".into(),
            args: Map::new(),
        })
        .await
        .expect("shutdown");
    daemon.await.expect("task").expect("daemon");
}
