use cccc_contracts::Event;
use cccc_core::{GroupStore, HomeLayout, ledger};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

pub fn find(
    home: &HomeLayout,
    group_id: &str,
    kind: &str,
    by: &str,
    args: &Map<String, Value>,
) -> Option<Event> {
    let client_id = args
        .get("client_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    let path = GroupStore::new(home.clone())
        .ok()?
        .ledger_path(group_id)
        .ok()?;
    ledger::find_idempotent(&path, kind, by, client_id)
        .ok()
        .flatten()
}

pub fn find_relay(home: &HomeLayout, group_id: &str, source_event_id: &str) -> Option<Event> {
    let path = GroupStore::new(home.clone())
        .ok()?
        .ledger_path(group_id)
        .ok()?;
    ledger::find_relay(&path, source_event_id).ok().flatten()
}

/// Return one owned cancellation after releasing the borrowed ledger view.
pub(super) fn find_cancellation(
    path: &std::path::Path,
    source_event_id: &str,
) -> std::io::Result<Option<Event>> {
    ledger::inspect(path, |events, _| {
        events
            .iter()
            .find(|event| {
                event.kind == "chat.reply_request.cancelled"
                    && event.data.get("source_event_id").and_then(Value::as_str)
                        == Some(source_event_id)
            })
            .cloned()
    })
}

pub fn tracked_client_id(group_id: &str, by: &str, key: &str) -> String {
    let digest = Sha256::digest(format!("{group_id}\0{by}\0{key}"));
    format!("tracked-send:{digest:.32x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captured_cancellations_borrow_history_and_release_it_before_append() {
        const TEST: &str = "ops::message_idempotency::tests::captured_cancellations_borrow_history_and_release_it_before_append";
        const CHILD: &str = "CCCC_TEST_CANCELLATION_LOOKUP_CHILD";
        if std::env::var(CHILD).as_deref() != Ok("1") {
            let output =
                std::process::Command::new(std::env::current_exe().expect("test executable"))
                    .args(["--exact", TEST, "--nocapture"])
                    .env(CHILD, "1")
                    .env("CCCC_LEDGER_INDEX_PROBE", "1")
                    .output()
                    .expect("isolated replay");
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let stderr = String::from_utf8_lossy(&output.stderr);
            let probes: Vec<Value> = stderr
                .lines()
                .filter_map(|line| line.strip_prefix("CCCC_EVENT_INDEX_QUERY "))
                .map(|line| serde_json::from_str(line).expect("query probe"))
                .collect();
            assert!(
                probes.iter().any(|probe| probe["api"] == "inspect"),
                "the replay must exercise a borrowed lookup"
            );
            assert!(
                probes.iter().all(|probe| probe["api"] != "read_all"),
                "cancellation lookup must not clone the complete captured history"
            );
            return;
        }
        let events: Vec<Event> = serde_json::from_str(include_str!(
            "../../tests/fixtures/captured_cancellations.json"
        ))
        .expect("captured cancellation history");
        let dir = tempfile::tempdir().expect("directory");
        let path = dir.path().join("ledger.jsonl");
        let mut bytes = Vec::new();
        for event in &events {
            serde_json::to_writer(&mut bytes, event).expect("event");
            bytes.push(b'\n');
        }
        std::fs::write(&path, bytes).expect("captured ledger");
        for expected in events
            .iter()
            .filter(|event| event.kind == "chat.reply_request.cancelled")
        {
            let source = expected.data["source_event_id"]
                .as_str()
                .expect("source id");
            assert_eq!(
                find_cancellation(&path, source)
                    .expect("first lookup")
                    .as_ref(),
                Some(expected)
            );
            assert_eq!(
                find_cancellation(&path, source)
                    .expect("repeated lookup")
                    .as_ref(),
                Some(expected)
            );
            assert!(
                find_cancellation(&path, &expected.id)
                    .expect("absent target")
                    .is_none()
            );
        }
        assert_eq!(
            std::fs::read_to_string(&path)
                .expect("ledger")
                .lines()
                .count(),
            events.len(),
            "lookups must not write receipts"
        );
        ledger::append(&path, &events[0]).expect("append after lookup must own no reader guard");
        assert_eq!(
            std::fs::read_to_string(&path)
                .expect("ledger")
                .lines()
                .count(),
            events.len() + 1
        );
    }

    #[test]
    fn tracked_client_id_matches_python_shape() {
        let value = tracked_client_id("group", "user", "request");
        assert_eq!(value, "tracked-send:abed96f07487e072ead638feb064bf1a");
    }
}
