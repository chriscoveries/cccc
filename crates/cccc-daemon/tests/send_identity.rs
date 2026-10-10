//! The `send` / `message_send` op is the only caller-facing hop that stamps an
//! arbitrary `by` into the group ledger. These tests pin the send-identity
//! rule: `by` must be blank, `user`, `system`, an actor of the target group
//! (`actors::find`), or a `connect:*` peer name — plus the daemon's own
//! `nomcp-advisory` writer. The identity that is validated is also the identity
//! that is recorded: a `by` with surrounding whitespace must not land in the
//! ledger as a name `actors::find` would no longer match.

use cccc_contracts::{DaemonRequest, DaemonResponse};
use cccc_core::HomeLayout;
use serde_json::{Map, Value, json};

fn call_raw(home: &HomeLayout, op: &str, args: Value) -> DaemonResponse {
    let request = DaemonRequest {
        v: 1,
        op: op.into(),
        args: args.as_object().cloned().unwrap_or_else(Map::new),
    };
    cccc_daemon::handle_request(home, &request)
}

fn call(home: &HomeLayout, op: &str, args: Value) -> DaemonResponse {
    let response = call_raw(home, op, args);
    assert!(
        response.ok,
        "{op} failed: {:?}",
        response.error.as_ref().map(|error| &error.message)
    );
    response
}

fn fixture(title: &str) -> (tempfile::TempDir, HomeLayout, String) {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    let created = call(
        &home,
        "group_create",
        json!({ "title": title, "by": "user" }),
    );
    let group_id = created.result["group"]["group_id"]
        .as_str()
        .expect("group id")
        .to_owned();
    call(
        &home,
        "actor_add",
        json!({
            "group_id": group_id,
            "actor_id": "peer1",
            "runtime": "custom",
            "command": ["sh", "-c", "sleep 30"],
            "by": "user"
        }),
    );
    call(
        &home,
        "actor_add",
        json!({
            "group_id": group_id,
            "actor_id": "peer2",
            "runtime": "custom",
            "command": ["sh", "-c", "sleep 30"],
            "by": "user"
        }),
    );
    (temp, home, group_id)
}

fn send(home: &HomeLayout, group_id: &str, by: &str, text: &str) -> DaemonResponse {
    // A sender is excluded from its own recipients, so send to a peer the
    // sender is not: the audience rule is a different guard than this one.
    let to = if by == "peer1" { "peer2" } else { "peer1" };
    call_raw(
        home,
        "message_send",
        json!({
            "group_id": group_id,
            "by": by,
            "to": [to],
            "text": text,
            "message_mode": "send"
        }),
    )
}

fn ledger_count(home: &HomeLayout, group_id: &str, by: &str) -> usize {
    let found = call(
        home,
        "ledger_search",
        json!({ "group_id": group_id, "by": by, "q": "identity check", "limit": 50 }),
    );
    found.result["events"]
        .as_array()
        .map(|events| events.len())
        .unwrap_or(0)
}

/// Stock behaviour: the op appends the event with whatever `by` the caller
/// sent, so a local process can forge a sender. The fix refuses at the op
/// boundary instead.
#[test]
fn send_op_refuses_a_spoofed_sender_and_appends_nothing() {
    let (_temp, home, group_id) = fixture("send-identity-refuse");
    let rejected = send(&home, &group_id, "mallory", "identity forged by mallory");
    let error = rejected
        .error
        .as_ref()
        .unwrap_or_else(|| panic!("refused send must carry an error, got: {rejected:?}"));
    assert!(!rejected.ok, "spoofed send must not report ok");
    assert_eq!(error.code, "permission_denied");
    assert!(
        error.message.contains("unknown actor: mallory"),
        "error message shape {}",
        error.message
    );
    assert_eq!(
        ledger_count(&home, &group_id, "mallory"),
        0,
        "a refused send must leave no chat.message attributed to mallory"
    );
}

/// The identities that must keep working, whatever the gate does. `nomcp-advisory`
/// is the web UI's no-MCP reply sender and `connect:*` is inbound Connect —
/// both are non-members by design and are allowlisted.
#[test]
fn send_op_accepts_the_identities_it_must_keep_accepting() {
    let (_temp, home, group_id) = fixture("send-identity-accept");
    for (by, text) in [
        ("user", "identity check: sent as the human operator"),
        ("system", "identity check: sent as the system"),
        ("peer1", "identity check: sent by a member actor"),
        (
            "connect:inst-7",
            "identity check: inbound connect peer name",
        ),
        (
            "nomcp-advisory",
            "identity check: web no-MCP advisory reply",
        ),
    ] {
        let response = send(&home, &group_id, by, text);
        assert!(
            response.ok,
            "by={by} must be accepted: {:?}",
            response.error.as_ref().map(|error| &error.message)
        );
        assert_eq!(
            ledger_count(&home, &group_id, by),
            1,
            "by={by} recorded once"
        );
    }

    // Omitted `by` still defaults to the user server-side; blank is the same case.
    let omitted = call_raw(
        &home,
        "message_send",
        json!({
            "group_id": group_id,
            "to": ["peer1"],
            "text": "identity check: no by field at all",
            "message_mode": "send"
        }),
    );
    assert!(omitted.ok, "omitted by must default to user");
    let blank = send(&home, &group_id, "", "identity check: blank by field");
    assert!(
        blank.ok,
        "blank by must be accepted: {:?}",
        blank.error.as_ref().map(|error| &error.message)
    );
}

/// The cross-group relay is a different op with its own source check
/// (`send_cross_group` at messaging.rs:103); the guard must not touch it.
#[test]
fn cross_group_send_still_relays() {
    let (_temp, home, source_id) = fixture("send-identity-src");
    let created = call(
        &home,
        "group_create",
        json!({ "title": "send-identity-dst", "by": "user" }),
    );
    call(
        &home,
        "actor_add",
        json!({
            "group_id": created.result["group"]["group_id"],
            "actor_id": "dst-lead",
            "runtime": "custom",
            "command": ["sh", "-c", "sleep 30"],
            "by": "user"
        }),
    );
    let destination_id = created.result["group"]["group_id"]
        .as_str()
        .expect("destination group id")
        .to_owned();
    let relayed = call(
        &home,
        "send_cross_group",
        json!({
            "group_id": source_id,
            "dst_group_id": destination_id,
            "by": "user",
            "text": "relay me across groups",
            "message_mode": "send"
        }),
    );
    assert!(
        relayed.result.get("source_event").is_some() || relayed.result.get("event").is_some(),
        "relay result shape {:?}",
        relayed.result
    );
}

/// Inbound Connect deliveries are written with the synthetic `connect:*` name
/// through the same `send`; the ledger must keep recording it verbatim so the
/// remote instance stays recognisable.
#[test]
fn connect_sender_is_recorded_verbatim() {
    let (_temp, home, group_id) = fixture("send-identity-connect");
    let response = send(
        &home,
        &group_id,
        "connect:peer-box-1",
        "delivered from a connect peer",
    );
    assert!(
        response.ok,
        "connect: sender must be accepted: {:?}",
        response.error.as_ref().map(|error| &error.message)
    );
    let events = call(
        &home,
        "ledger_search",
        json!({ "group_id": group_id, "by": "connect:peer-box-1", "q": "connect peer", "limit": 10 }),
    );
    let by = events.result["events"][0]["by"].as_str().expect("event by");
    assert_eq!(
        by, "connect:peer-box-1",
        "the connect name must not be rewritten"
    );
}

/// The gate validates a trimmed `by`, so the ledger must record that same
/// trimmed identity. Before the canonicalisation fix this landed as " peer1 ",
/// which `actors::find` no longer matches — a name the gate exists to prevent.
#[test]
fn send_records_the_validated_identity_not_the_raw_whitespace_form() {
    let (_temp, home, group_id) = fixture("identity-normalization");
    for op in ["send", "message_send"] {
        let response = call_raw(
            &home,
            op,
            json!({
                "group_id": group_id,
                "by": " peer1 ",
                "to": ["peer2"],
                "text": "identity normalization boundary",
                "message_mode": "mail"
            }),
        );
        assert!(
            response.ok,
            "a member with surrounding whitespace should be accepted: {:?}",
            response.error
        );
        assert_eq!(
            response.result["event"]["by"], "peer1",
            "the validated member must be the identity recorded in the ledger"
        );
    }
}

/// A whitespace-only `by` is not an identity; it must fall back to the "user"
/// default rather than recording an empty sender.
#[test]
fn send_records_the_user_default_when_by_is_only_whitespace() {
    let (_temp, home, group_id) = fixture("identity-blank");
    let response = call_raw(
        &home,
        "send",
        json!({
            "group_id": group_id,
            "by": "   ",
            "to": ["peer2"],
            "text": "blank identity",
            "message_mode": "mail"
        }),
    );
    assert!(response.ok, "blank identity should default, not fail");
    assert_eq!(response.result["event"]["by"], "user");
}
