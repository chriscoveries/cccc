use anyhow::Result;
use cccc_client::DaemonClient;
use cccc_contracts::DaemonRequest;
use cccc_core::HomeLayout;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

mod installation;

pub async fn run(home: &HomeLayout, product_version: &str, all_runtimes: bool) -> Result<()> {
    let browser = cccc_web::system_browser_path();
    let xvfb = find_command("Xvfb");
    let x11vnc = find_command("x11vnc");
    let daemon = daemon_status(home).await;
    let sessions = runtime_sessions(home).await;
    let report = report(
        home,
        product_version,
        browser.as_deref(),
        xvfb.as_deref(),
        x11vnc.as_deref(),
        daemon,
        sessions,
        all_runtimes,
    );
    println!("{}", serde_json::to_string_pretty(&report)?);
    // A crossed or stale terminal is the failure this check exists for: the
    // report has to be able to fail a script, not only be read by a human.
    let code = identity_exit_code(&report["runtime_sessions"]);
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

async fn runtime_sessions(home: &HomeLayout) -> Value {
    let client = DaemonClient::new(home.clone()).with_timeout(Duration::from_millis(750));
    let request = DaemonRequest {
        v: 1,
        op: "runtime_sessions".into(),
        args: Default::default(),
    };
    match client.call(&request).await {
        Ok(response) if response.ok => Value::Object(response.result),
        Ok(response) => json!({
            "unavailable": response
                .error
                .map(|error| error.message)
                .unwrap_or_else(|| "daemon refused runtime_sessions".to_owned()),
        }),
        Err(error) => json!({"unavailable": error.to_string()}),
    }
}

/// Compare the key each session is registered under with the identity its
/// process reports. A session whose environment cannot be read is reported but
/// is not a mismatch; a running session with no process behind it is its own
/// finding, because it will silently swallow every delivery aimed at it.
fn identity_report(sessions: &Value) -> Value {
    let Some(sessions) = sessions.get("sessions").and_then(Value::as_array) else {
        let note = sessions
            .get("unavailable")
            .and_then(Value::as_str)
            .map(|reason| format!("daemon did not answer ({reason})"))
            .unwrap_or_else(|| "no registered runtime sessions".to_owned());
        return json!({
            "checked": 0,
            "healthy": true,
            "crossed": [],
            "stalled": [],
            "note": note,
        });
    };
    let mut crossed = Vec::new();
    let mut stalled = Vec::new();
    let mut unreadable = 0_usize;
    for session in sessions {
        let group_id = session
            .get("group_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let actor_id = session
            .get("actor_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let pid = session.get("pid").cloned().unwrap_or(Value::Null);
        let running = session
            .get("running")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let alive = session
            .get("process_alive")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        if running && !alive {
            stalled.push(json!({"group_id":group_id,"actor_id":actor_id,"pid":pid}));
        }
        match session.get("identity").and_then(Value::as_object) {
            None => unreadable += 1,
            Some(found) => {
                let found_group = found
                    .get("group_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let found_actor = found
                    .get("actor_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if found_group != group_id || found_actor != actor_id {
                    crossed.push(json!({
                        "group_id":group_id,
                        "actor_id":actor_id,
                        "pid":pid,
                        "found":{"group_id":found_group,"actor_id":found_actor},
                    }));
                }
            }
        }
    }
    let healthy = crossed.is_empty() && stalled.is_empty();
    let note = if sessions.is_empty() {
        "no registered runtime sessions".to_owned()
    } else if unreadable == 0 {
        String::new()
    } else if healthy {
        format!("{unreadable} session(s) do not report an identity; not a mismatch")
    } else {
        format!("{unreadable} session(s) do not report an identity")
    };
    json!({
        "checked": sessions.len(),
        "healthy": healthy,
        "crossed": crossed,
        "stalled": stalled,
        "note": note,
    })
}

fn identity_exit_code(runtime_sessions: &Value) -> i32 {
    match runtime_sessions.get("healthy").and_then(Value::as_bool) {
        Some(false) => 1,
        _ => 0,
    }
}

async fn daemon_status(home: &HomeLayout) -> Value {
    let client = DaemonClient::new(home.clone()).with_timeout(Duration::from_millis(750));
    let request = DaemonRequest {
        v: 1,
        op: "ping".into(),
        args: Default::default(),
    };
    match client.call(&request).await {
        Ok(response) if response.ok => json!({
            "running":true,
            "pid":response.result.get("pid").cloned().unwrap_or(Value::Null),
            "version":response.result.get("version").cloned().unwrap_or(Value::Null),
            "build":response.result.get("build").cloned().unwrap_or(Value::Null),
            "executable":response.result.get("executable").cloned().unwrap_or(Value::Null),
            "implementation":response.result.get("implementation").cloned().unwrap_or(Value::Null),
        }),
        Ok(response) => json!({
            "running":false,
            "error":response.error.map(|error| error.message),
        }),
        Err(error) => json!({"running":false,"error":error.to_string()}),
    }
}

fn report(
    home: &HomeLayout,
    product_version: &str,
    browser: Option<&Path>,
    xvfb: Option<&Path>,
    x11vnc: Option<&Path>,
    daemon: Value,
    sessions: Value,
    all_runtimes: bool,
) -> Value {
    let linux = cfg!(target_os = "linux");
    let mut runtimes = cccc_runtime::detect_runtimes();
    if !all_runtimes {
        runtimes.retain(|runtime| runtime.name != "custom");
    }
    json!({
        "version":product_version,
        "build":cccc_core::build_info::current(),
        "web_assets":cccc_web::web_assets_info().map(|info| info.assets_id),
        "home":home.root(),
        "installation":installation::report(),
        "daemon":daemon,
        "runtime_sessions":identity_report(&sessions),
        "runtimes":runtimes,
        "pty":{
            "supported":true,
            "backend":if cfg!(windows){"ConPTY"}else{"native PTY"},
        },
        "projected_browser":{
            "mode":"hybrid",
            "web_model_mode":"system_browser_cdp",
            "other_surface_mode":"headless",
            "browser_available":browser.is_some(),
            "browser_path":browser,
            "xvfb_required":linux,
            "system_browser_available":browser.is_some() && (!linux || xvfb.is_some()),
            "system_browser_path":browser,
            "xvfb_available":xvfb.is_some(),
            "xvfb_path":xvfb,
            "x11vnc_available":x11vnc.is_some(),
            "x11vnc_path":x11vnc,
            "xvfb_required_for_linux_web_model":linux,
            "note":if linux {
                "Web Model uses system Chrome/Edge/Chromium via an isolated Xvfb display; Xvfb is required on Linux."
            } else {
                "Web Model uses system Chrome/Edge/Chromium via CDP."
            }
        }
    })
}

fn find_command(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|directory| directory.join(name))
        .find(|path| path.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_hybrid_browser_contract() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let browser = Path::new("/usr/bin/google-chrome");
        let xvfb = Path::new("/usr/bin/Xvfb");
        let value = report(
            &home,
            "0.4.33",
            Some(browser),
            Some(xvfb),
            None,
            json!({"running":false}),
            json!({"sessions":[]}),
            false,
        );
        assert_eq!(value["version"], "0.4.33");
        assert!(value["installation"]["path_status"].is_string());
        assert!(value["installation"]["command_candidates"].is_array());
        assert_eq!(value["daemon"]["running"], false);
        assert_eq!(value["projected_browser"]["mode"], "hybrid");
        assert_eq!(
            value["projected_browser"]["web_model_mode"],
            "system_browser_cdp"
        );
        assert_eq!(value["projected_browser"]["other_surface_mode"], "headless");
        assert_eq!(value["projected_browser"]["browser_available"], true);
        assert_eq!(
            value["projected_browser"]["browser_path"],
            browser.to_string_lossy().as_ref()
        );
        assert_eq!(
            value["projected_browser"]["xvfb_required"],
            cfg!(target_os = "linux")
        );
        assert_eq!(value["projected_browser"]["system_browser_available"], true);
        assert_eq!(
            value["projected_browser"]["system_browser_path"],
            browser.to_string_lossy().as_ref()
        );
        assert_eq!(
            value["projected_browser"]["xvfb_required_for_linux_web_model"],
            cfg!(target_os = "linux")
        );
        assert!(
            value["runtimes"]
                .as_array()
                .expect("runtimes")
                .iter()
                .all(|runtime| runtime["name"] != "custom")
        );
    }

    #[test]
    fn linux_system_browser_contract_requires_xvfb() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let value = report(
            &home,
            "0.4.33",
            Some(Path::new("/usr/bin/chromium")),
            None,
            None,
            json!({"running":false}),
            json!({"sessions":[]}),
            true,
        );
        assert_eq!(
            value["projected_browser"]["system_browser_available"],
            !cfg!(target_os = "linux")
        );
        assert!(
            value["runtimes"]
                .as_array()
                .expect("runtimes")
                .iter()
                .any(|runtime| runtime["name"] == "custom")
        );
    }

    #[test]
    fn crossed_terminal_is_named_and_fails_the_run() {
        let value = identity_report(&json!({"sessions":[
            {"group_id":"g_a","actor_id":"peer1","pid":4242,"started_at":"2026-10-20T10:00:00Z",
             "running":true,"process_alive":true,"identity":{"group_id":"g_a","actor_id":"peer1"}},
            {"group_id":"g_a","actor_id":"peer2","pid":4343,"started_at":"2026-10-20T10:05:00Z",
             "running":true,"process_alive":true,"identity":{"group_id":"g_a","actor_id":"peer1"}},
        ]}));
        assert_eq!(value["checked"], 2);
        assert_eq!(value["healthy"], false);
        assert_eq!(identity_exit_code(&value), 1);
        let crossed = value["crossed"].as_array().expect("crossed");
        assert_eq!(crossed.len(), 1);
        assert_eq!(crossed[0]["group_id"], "g_a");
        assert_eq!(crossed[0]["actor_id"], "peer2");
        assert_eq!(crossed[0]["pid"], 4343);
        assert_eq!(crossed[0]["found"]["group_id"], "g_a");
        assert_eq!(crossed[0]["found"]["actor_id"], "peer1");
    }

    #[test]
    fn no_sessions_or_an_unreadable_environment_exits_zero() {
        let empty = identity_report(&json!({"sessions":[]}));
        assert_eq!(empty["healthy"], true);
        assert_eq!(empty["checked"], 0);
        assert_eq!(identity_exit_code(&empty), 0);
        assert_eq!(empty["note"], "no registered runtime sessions");

        let dark = identity_report(&json!({"sessions":[
            {"group_id":"g_a","actor_id":"peer1","pid":4242,"started_at":"2026-10-20T10:00:00Z",
             "running":true,"process_alive":true,"identity":null},
        ]}));
        assert_eq!(dark["healthy"], true);
        assert_eq!(identity_exit_code(&dark), 0);
        assert!(
            dark["note"]
                .as_str()
                .expect("note")
                .contains("do not report an identity")
        );

        let offline = identity_report(&json!({"unavailable":"connect: connection refused"}));
        assert_eq!(offline["healthy"], true);
        assert_eq!(identity_exit_code(&offline), 0);
        assert!(
            offline["note"]
                .as_str()
                .expect("note")
                .contains("did not answer")
        );
    }

    #[test]
    fn a_running_session_with_no_process_is_stalled() {
        let value = identity_report(&json!({"sessions":[
            {"group_id":"g_a","actor_id":"peer1","pid":999999,"started_at":"2026-10-20T10:00:00Z",
             "running":true,"process_alive":false,"identity":{"group_id":"g_a","actor_id":"peer1"}},
        ]}));
        assert_eq!(value["healthy"], false);
        assert_eq!(identity_exit_code(&value), 1);
        let stalled = value["stalled"].as_array().expect("stalled");
        assert_eq!(stalled.len(), 1);
        assert_eq!(stalled[0]["group_id"], "g_a");
        assert_eq!(stalled[0]["actor_id"], "peer1");
        assert_eq!(stalled[0]["pid"], 999999);
    }
}
