BRANCH: chriscoveries/cccc up/send-identity  (base: upstream/main d1ba8424)
DUP-CHECK: no upstream issue/PR on send-identity forging. Security-relevant; Suggest opening with the fix.

=== UPSTREAM ISSUE ===
Title: `send`/`message_send` accepts any `by` — local callers can forge ledger sender identity

The send op stamps whatever `by` the caller sent into the group ledger with no check. Any local process can append a `chat.message` attributed to an arbitrary identity: forged senders flip groups to Active via message_wake, poison inbox/read-state keys, and defeat (kind, by, client_id) idempotency so forged sends duplicate content.

=== PR BODY ===
Hi @ChesterRa — a sender-identity gate on the caller-facing send hop. Flagging security-relevance up front.

`send`/`message_send` stamped whatever `by` the caller sent straight into the group ledger — `let by = string_arg(request, "by").unwrap_or_else(|| "user".into())` with no check — so any local process could append a `chat.message` attributed to an arbitrary identity. Downstream that is forged sender identity in the ledger: `message_wake` treats a non-member name as external and flips the group to Active, inbox/read-state keys follow the forged name, and `message_idempotency` keys on (kind, by, client_id), so a forged sender duplicates content.

The gate mirrors the source check `send_cross_group` already does at its own boundary: `by` must be blank, "user", "system", an actor of the target group (`actors::find`, so internal actors pass too), a `connect:*` peer name, or the allowlisted nomcp-advisory writer the web UI uses for no-MCP replies. Anything else is refused `permission_denied` with the same "unknown actor: <name>" shape as send_cross_group.

A second commit fixes what the gate itself introduced: `by` was trimmed for the check but `send_with_audience_policy` re-read the raw value to stamp the event, so `" peer1 "` validated but was recorded verbatim — a name no later `actors::find` matches, i.e. exactly the forged-identity shape via a different route. `by` is now canonicalised once at the op boundary before `send()` reads it; whitespace-only falls back to "user".

Untouched on purpose: the internal `send()` helper, `send_files`, `reply`, `tracked_send`, `slash_skill_dispatch`, `system_notify`, and send_cross_group's destination leg — four legitimately write non-member names, so the check stays at the op boundary rather than inside `send_with_audience_policy`. Cross-group relays and inbound Connect keep working unchanged.

## How we built it

Base commit d1ba8424. Two commits: the gate + `tests/send_identity.rs`, then the canonicalisation fix found by our own review of the first. Boundary-only enforcement is deliberate — the daemon's internal writers legitimately use names that are not group actors.

## How we tested it

- `cargo test -p cccc-pair-daemon --test send_identity` : 6 passed, 0 failed — spoof refused (nothing appended), member/blank/user/system/connect:* still accepted, cross-group relay intact, whitespace recorded as the validated identity.
- Red→green: the spoof test fails on unpatched main.
- `cargo fmt`/`cargo clippy --all-targets -- -D warnings` on cccc-pair-daemon: clean.
- Test env: Skylake 8 vCPU, 15 GB, Debian 13. In production on firestar (build 0990769c) since 2026-10-10 across a multi-lane fleet that sends thousands of messages a day.
