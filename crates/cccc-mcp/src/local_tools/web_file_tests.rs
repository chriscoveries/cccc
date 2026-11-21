//! Web Model file-tool confinement: the configured public directory and
//! conversation-owned blobs, through the real file and send handlers.
use cccc_client::DaemonClient;
use cccc_contracts::{DaemonRequest, Event};
use cccc_core::{GroupStore, HomeLayout};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::authority_tests::Fixture;
use super::{
    WebFileRoot, file, file_path, send_files_scoped, web_blob_attached, web_file_path,
    web_send_paths,
};

/// A public staging directory inside the caller's effective scope, so sends
/// also pass the daemon's own scope check.
fn staging(fixture: &Fixture) -> PathBuf {
    let dir = fixture.root.join("public");
    std::fs::create_dir_all(&dir).expect("staging directory");
    dir.canonicalize().expect("canonical staging")
}

fn root_of(dir: &Path) -> WebFileRoot {
    WebFileRoot::Root(dir.canonicalize().expect("canonical staging"))
}

fn ledger_bytes(fixture: &Fixture) -> Vec<u8> {
    let ledger = GroupStore::new(fixture.home.clone())
        .and_then(|store| store.ledger_path(&fixture.group_id))
        .expect("ledger");
    std::fs::read(ledger).unwrap_or_default()
}

fn store_blob(fixture: &Fixture, data: &[u8]) -> cccc_core::blobs::BlobInfo {
    cccc_core::blobs::store(&fixture.home, &fixture.group_id, data).expect("blob")
}

fn attach(fixture: &Fixture, sender: &str, to: Value, blob_path: &str) {
    let mut event = Event::new("chat.message", &fixture.group_id);
    event.by = sender.into();
    event.data.insert("to".into(), to);
    event.data.insert(
        "attachments".into(),
        json!([{"path":blob_path,"title":"f","bytes":1,"sha256":"0".repeat(64)}]),
    );
    let ledger = GroupStore::new(fixture.home.clone())
        .and_then(|store| store.ledger_path(&fixture.group_id))
        .expect("ledger");
    cccc_core::ledger::append(&ledger, &event).expect("append");
}

#[test]
fn web_reads_stay_inside_the_configured_public_directory() {
    let fixture = Fixture::new();
    let public = staging(&fixture);
    std::fs::write(public.join("note.txt"), "public note").expect("staged file");
    let args = fixture.args(json!({}));
    let root = root_of(&public);

    // Relative and absolute spellings inside the directory resolve to it.
    for raw in [
        "note.txt".to_owned(),
        public.join("note.txt").to_string_lossy().into_owned(),
    ] {
        let path = file_path(&fixture.home, &args, &fixture.root, &raw, &root)
            .expect("inside the public directory");
        assert_eq!(
            path,
            public.join("note.txt").canonicalize().expect("canonical")
        );
    }

    // Scope files, traversal, absolute escapes and missing names are refused.
    for raw in [
        "private.txt".to_owned(),
        "../private.txt".into(),
        "sub/../../private.txt".into(),
        fixture
            .root
            .join("private.txt")
            .to_string_lossy()
            .into_owned(),
        "/etc/hostname".into(),
        "missing.txt".into(),
    ] {
        assert!(
            file_path(&fixture.home, &args, &fixture.root, &raw, &root).is_err(),
            "{raw} must be refused"
        );
    }

    #[cfg(unix)]
    {
        let outside = fixture.root.parent().expect("temp").join("outside.txt");
        std::fs::write(&outside, "outside").expect("outside file");
        std::os::unix::fs::symlink(&outside, public.join("escape.txt")).expect("symlink");
        assert!(
            file_path(&fixture.home, &args, &fixture.root, "escape.txt", &root).is_err(),
            "a symlink landing outside the directory must be refused"
        );
        std::os::unix::fs::symlink(public.join("note.txt"), public.join("alias.txt"))
            .expect("inside symlink");
        assert!(
            file_path(&fixture.home, &args, &fixture.root, "alias.txt", &root).is_ok(),
            "a symlink staying inside the directory resolves"
        );
    }
}

#[test]
fn unconfigured_and_invalid_settings_both_deny_web_file_tools() {
    let fixture = Fixture::new();
    let args = fixture.args(json!({}));
    // Unset: a Web Model caller is denied even a scope file — the setting is
    // required, never an effective-scope fallback.
    for web_root in [WebFileRoot::Unset, WebFileRoot::Invalid] {
        let error = file_path(
            &fixture.home,
            &args,
            &fixture.root,
            "private.txt",
            &web_root,
        )
        .expect_err("unconfigured or invalid setting denies");
        assert!(
            error.starts_with("permission_denied:"),
            "{web_root:?}: {error}"
        );
        let raws = ["private.txt"];
        assert!(
            web_send_paths(&args, &raws, &web_root).is_err(),
            "{web_root:?}: sends deny too"
        );
    }
    // Own-conversation blobs stay readable with no root configured: the
    // blob branch never consults the file root.
    let blob = store_blob(&fixture, b"own bytes");
    attach(&fixture, "dots", json!([]), &blob.path);
    let name = blob.path.strip_prefix("state/blobs/").expect("blob name");
    let path = file_path(
        &fixture.home,
        &args,
        &fixture.root,
        &format!("state/blobs/{name}"),
        &WebFileRoot::Unset,
    )
    .expect("own blob resolves without a file root");
    assert!(path.ends_with(name));
}

#[test]
fn non_web_callers_ignore_the_web_file_root() {
    let fixture = Fixture::new();
    let public = staging(&fixture);
    std::fs::write(public.join("note.txt"), "public note").expect("staged file");
    // No binding: scope resolution applies even when a root is supplied.
    let mut args = fixture.args(json!({}));
    args.remove("_cccc_web_binding");
    let path = file_path(
        &fixture.home,
        &args,
        &fixture.root,
        "private.txt",
        &root_of(&public),
    )
    .expect("non-web caller keeps scope resolution");
    assert!(path.ends_with("private.txt"));
    // The same holds for send path selection.
    let raws = ["private.txt"];
    assert!(
        web_send_paths(&args, &raws, &root_of(&public))
            .expect("non-web send paths")
            .is_none()
    );
}

#[tokio::test]
async fn web_blob_reads_require_conversation_ownership() {
    let fixture = Fixture::new();
    let client = DaemonClient::new(fixture.home.clone());
    let own = store_blob(&fixture, b"own conversation bytes");
    let foreign = store_blob(&fixture, b"foreign conversation bytes");

    // Sent by the actor, addressed to it, and broadcast all count as its
    // conversation; a private message to someone else does not.
    attach(&fixture, "dots", json!([]), &own.path);
    attach(&fixture, "user", json!(["dots"]), &foreign.path);
    let private = store_blob(&fixture, b"someone else's bytes");
    attach(&fixture, "user", json!(["reviewer"]), &private.path);

    for (blob, expect) in [(&own, true), (&foreign, true), (&private, false)] {
        let name = blob.path.strip_prefix("state/blobs/").expect("blob name");
        assert_eq!(
            web_blob_attached(&fixture.home, &fixture.group_id, "dots", name)
                .expect("ownership scan"),
            expect,
            "{}",
            blob.path
        );
    }

    // The real read handler admits the owned blob for every read action.
    for action in ["read", "info", "blob_path"] {
        let args = fixture.args(json!({"path": own.path.as_str(), "action": action}));
        let result = file(&fixture.home, &client, &fixture.root, &args, action)
            .await
            .expect("own blob admitted");
        if action == "read" {
            assert!(result.to_string().contains("own conversation bytes"));
        }
    }
    for action in ["read", "info", "blob_path"] {
        let args = fixture.args(json!({"path": private.path.as_str(), "action": action}));
        let error = file(&fixture.home, &client, &fixture.root, &args, action)
            .await
            .expect_err("foreign blob refused");
        assert_eq!(error.code, "permission_denied", "{action}");
    }
    // A blob nobody attached is refused the same way.
    let stray = store_blob(&fixture, b"never delivered");
    let args = fixture.args(json!({"path": stray.path.as_str()}));
    assert!(
        file(&fixture.home, &client, &fixture.root, &args, "info")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn non_web_blob_reads_keep_group_access() {
    let fixture = Fixture::new();
    let client = DaemonClient::new(fixture.home.clone());
    let blob = store_blob(&fixture, b"group blob bytes");
    // No conversation owns it, but a caller without the binding still reads
    // any blob in the group, exactly as before.
    let mut args = fixture.args(json!({"path": blob.path.as_str()}));
    args.remove("_cccc_web_binding");
    let result = file(&fixture.home, &client, &fixture.root, &args, "read")
        .await
        .expect("non-web blob read");
    assert!(result.to_string().contains("group blob bytes"));
}

struct Ipc {
    client: DaemonClient,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Ipc {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn ipc(home: &HomeLayout) -> Ipc {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener");
    let paths = cccc_daemon::DaemonPaths::new(home.clone());
    cccc_core::fs::write_json(
        &paths.address,
        &json!({"v":1,"transport":"tcp","path":"",
            "host":"127.0.0.1","port":listener.local_addr().expect("address").port(),
            "pid":std::process::id(),"version":env!("CARGO_PKG_VERSION"),
            "ts":cccc_contracts::utc_now()}),
    )
    .expect("IPC address");
    let server_home = home.clone();
    let server = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.expect("accept");
            let mut stream = BufReader::new(stream);
            let mut line = String::new();
            stream.read_line(&mut line).await.expect("request");
            let request: DaemonRequest = serde_json::from_str(&line).expect("JSON request");
            let response = cccc_daemon::handle_request(&server_home, &request);
            let mut bytes = serde_json::to_vec(&response).expect("JSON response");
            bytes.push(b'\n');
            stream.get_mut().write_all(&bytes).await.expect("response");
        }
    });
    Ipc {
        client: DaemonClient::new(home.clone()),
        server,
    }
}

#[tokio::test]
async fn web_send_inside_the_public_directory_reaches_the_daemon() {
    let fixture = Fixture::new();
    fixture.home.initialize().expect("initialize");
    let public = staging(&fixture);
    std::fs::write(public.join("shot.png"), "staged png").expect("file");
    let ipc = ipc(&fixture.home).await;
    let root = root_of(&public);
    let args = fixture.args(json!({
        "path": ["shot.png"], "to": ["user"], "mode": "send",
        "text": "staged", "insight": "fixture"
    }));
    let sent = send_files_scoped(&ipc.client, &fixture.root, &args, &root)
        .await
        .expect("send inside the public directory");
    assert_eq!(sent["structuredContent"]["sent"], true);
    assert_eq!(
        sent["structuredContent"]["attachments"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
}

#[tokio::test]
async fn web_send_rejects_the_whole_request_when_any_path_is_outside() {
    let fixture = Fixture::new();
    fixture.home.initialize().expect("initialize");
    let public = staging(&fixture);
    std::fs::write(public.join("ok.txt"), "ok").expect("file");
    // The daemon must never be consulted: a dummy client proves the refusal
    // happens before any send branch runs.
    let client = DaemonClient::new(fixture.home.clone());
    let root = root_of(&public);
    let before = ledger_bytes(&fixture);
    for bad in [
        "../private.txt".to_owned(),
        fixture
            .root
            .join("private.txt")
            .to_string_lossy()
            .into_owned(),
        "sub/../../private.txt".into(),
    ] {
        let args = fixture.args(json!({
            "path": ["ok.txt", bad], "to": ["user"], "mode": "send",
            "text": "mixed", "insight": "fixture"
        }));
        send_files_scoped(&client, &fixture.root, &args, &root)
            .await
            .expect_err("one bad path refuses the whole send");
    }
    #[cfg(unix)]
    {
        let outside = fixture.root.parent().expect("temp").join("outside.txt");
        std::fs::write(&outside, "outside").expect("outside file");
        std::os::unix::fs::symlink(&outside, public.join("escape.txt")).expect("symlink");
        let args = fixture.args(json!({
            "path": ["ok.txt", "escape.txt"], "to": ["user"], "mode": "send",
            "text": "mixed", "insight": "fixture"
        }));
        send_files_scoped(&client, &fixture.root, &args, &root)
            .await
            .expect_err("a symlink escape refuses the whole send");
    }
    // Stored blobs are never send sources for a Web Model caller.
    let args = fixture.args(json!({
        "path": ["state/blobs/not-a-real-blob"], "to": ["user"], "mode": "send",
        "text": "mixed", "insight": "fixture"
    }));
    send_files_scoped(&client, &fixture.root, &args, &root)
        .await
        .expect_err("blob sources are refused");
    assert_eq!(
        ledger_bytes(&fixture),
        before,
        "no refused send may reach the ledger"
    );
}

#[test]
fn web_file_path_admits_missing_sources_only_for_sends() {
    let fixture = Fixture::new();
    let public = staging(&fixture);
    let root = root_of(&public);
    // Reads require an existing file; sends keep the daemon's retry lookup.
    assert!(web_file_path(&public, "new.txt", false).is_err());
    assert_eq!(
        web_file_path(&public, "new.txt", true).expect("send source"),
        public.join("new.txt")
    );
    assert!(web_file_path(&public, "../new.txt", true).is_err());
    #[cfg(unix)]
    {
        // A missing file beneath a directory that leaves the public root is
        // refused even though the file itself does not exist yet.
        let outside_dir = fixture.root.join("private_dir");
        std::fs::create_dir(&outside_dir).expect("outside dir");
        std::os::unix::fs::symlink(&outside_dir, public.join("link")).expect("link");
        assert!(web_file_path(&public, "link/new.txt", true).is_err());
        // A symlink staying inside the directory still admits a send source.
        let inner = public.join("inner");
        std::fs::create_dir(&inner).expect("inner dir");
        std::os::unix::fs::symlink(&inner, public.join("inner_link")).expect("inner link");
        assert_eq!(
            web_file_path(&public, "inner_link/new.txt", true).expect("inner send source"),
            inner.join("new.txt")
        );
    }
}
