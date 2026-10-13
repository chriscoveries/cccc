REDRAFT per seat ruling: NOT a public issue/PR — GitHub private security advisory text per ChesterRa/cccc SECURITY.md. Branch chriscoveries/cccc up/send-identity is the suggested fix, offered inside the advisory. Posts only on owner's go.

=== PRIVATE SECURITY ADVISORY (draft) ===
Title: send/message_send accepts arbitrary `by` — local callers can forge ledger sender identity
Severity suggestion: moderate (local-process spoofing; no remote surface, but corrupts the shared audit ledger)

**Affected version and environment**
Observed on main through d1ba8424 and in production deployment of 0.4.42-rc builds (Linux). Any cccc deployment where a non-daemon local process can reach the daemon socket.

**Reproduction steps**
Call the `send`/`message_send` op with a `by` naming any actor — including an actor of another group or a name that does not exist:

    cccc daemon ... op send '{"group_id":"<g>","by":"foreman","to":["victim"],"text":"…"}'

`messaging.rs` stamped `by` verbatim into the group ledger (`let by = string_arg(request, "by").unwrap_or_else(|| "user".into())` with no check). The event lands as a `chat.message` authored by the forged identity.

**Impact assessment**
- Forged sender identity in the durable ledger: `message_wake` treats a non-member name as external and flips the group to Active, so a forged send wakes groups and lanes as if a member spoke.
- Inbox/read-state keys follow the forged name.
- `message_idempotency` keys on (kind, by, client_id): a forged sender bypasses dedupe and can duplicate content.
- No remote/unauthenticated surface — a caller needs local daemon access — but the ledger is the system's audit of record, so attribution integrity matters beyond the sender check itself.

**Suggested mitigation**
Gate `by` at the caller-facing op boundary, mirroring the source check `send_cross_group` already performs: blank, "user", "system", an actor of the target group (`actors::find`, internal actors included), a `connect:*` peer name, or the allowlisted nomcp-advisory writer pass; anything else is refused `permission_denied` ("unknown actor: <name>").

A second commit is required alongside the gate: canonicalise `by` once at the boundary before `send()` reads it — the first-pass gate trimmed for the check while the ledger stamped the raw string, so `" peer1 "` validated but was recorded verbatim (the same forged-identity shape via a different route).

Internal writers (`send()` helper, `send_files`, `reply`, `tracked_send`, `slash_skill_dispatch`, `system_notify`, and the cross-group destination leg) legitimately use non-member names and are deliberately untouched; cross-group relays and inbound Connect keep working.

A tested patch is available on request (or we can open it as a PR after triage, whichever you prefer):
- branch `up/send-identity` on github.com/chriscoveries/cccc, based on main d1ba8424
- regression tests in `crates/cccc-daemon/tests/send_identity.rs`: spoof refused with nothing appended; member/blank/system/connect:* accepted; cross-group relay intact; whitespace canonicalised
- `cargo test -p cccc-pair-daemon --test send_identity`: 6 pass; spoof test is red on unpatched main; fmt + `clippy --all-targets -D warnings` clean
- Running in our production fleet (firestar, 0990769c) since 2026-10-10 across a multi-lane fleet sending thousands of messages/day
=== END ADVISORY ===
