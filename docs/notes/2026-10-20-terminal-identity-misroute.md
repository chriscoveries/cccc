# Terminal identity misroute (19 Oct 2026, JAMES)

## What happened

Two lanes, `j-hermes-a` and `j-hermes-b`, were both sent the *same* payload at
17:31:46Z on 19 October. The evidence is in the live records, not a transcript:

- `~/.hermes/state.db` sessions `20261019_171529_d48013` (a) and
  `20261019_171539_e5d836` (b) both begin with the identical 1921-character
  user message, `sha256 62308d0e51b308b2…`, opening
  `[CCCC] You are j-hermes-b (peer)` and carrying b's task ("90 new library
  admissions"). Byte-identical: this was not a mislabelled header.
- The group ledger `g_e28e1ec9e482` records two *separate* deliveries at
  17:31:44 — `delivery:j-hermes-a:63ce0316…` and `delivery:j-hermes-b:64388e14…`
  — each `claimed` and then `accepted` about 1.5 s later.
- Both lanes then posted through MCP as `j-hermes-b` and clobbered
  `HERMES_B.tsv` three times. `j-hermes-a`'s own card was never started.
- `j-hermes-a` and `j-hermes-b` have byte-identical actor specs in
  `group.yaml` (same command, `env: {}`), created 48 ms apart. Neither carries
  an explicit `CCCC_ACTOR_ID`; the launcher supplies it.

## The write path

```
actor_delivery_worker::process_batch            crates/cccc-daemon/src/ops/actor_delivery_worker.rs
  -> actor_delivery::submit_terminal_text       crates/cccc-daemon/src/ops/actor_delivery.rs:29
    -> cccc_runtime::submit_sequence_interruptible   crates/cccc-runtime/src/manager.rs:203
      -> registry::lookup(group_id, actor_id)   crates/cccc-runtime/src/registry.rs:19
        -> write_locked(...)                    crates/cccc-runtime/src/manager.rs:281
```

The registry key `(group_id, actor_id)` is set once, at
`manager::start_inner` (`manager.rs:31-46`) from `LaunchSpec`, and it is
compared to the session's own status only by construction. `Session::start`
pins `CCCC_GROUP_ID` and `CCCC_ACTOR_ID` *last*, after any `spec.env`
(`session.rs:84-85`), so a running process's identity is whatever the
`LaunchSpec` said — it can never drift after launch.

That is exactly why the incident was silent. Nothing on the write path asks the
process who it is:

1. `lookup` returns the session registered under the job's actor.
2. `write_locked` writes. Success means *the pty accepted bytes*, not that the
   recipient is the intended actor.
3. The worker records the delivery as `accepted` and finishes the job.

So a terminal launched for the wrong actor — or a lane launched by hand with a
stale `CCCC_ACTOR_ID` — receives another actor's work, records it as delivered,
and runs for hours under the wrong identity with no error anywhere.

## Why this shape, and what remains open

Because the launcher overrides `spec.env`, a pty cannot acquire an identity
after launch. A mismatch therefore means the `LaunchSpec` itself carried the
other actor's id, or the lane was started outside the daemon with that
environment exported. The daemon keeps no launch record — no `(delivery,
session, pid)` line at claim time — so the two cannot be distinguished after
the fact from the durable evidence alone. The foreman has previously recorded
the same class from the seat side ("routing was correct, only session-header
text mislabelled every seat in the wave"), which points at fan-out tooling that
composes one payload per wave; that is a hypothesis, not a finding.

## The fix (this branch)

`Session::process_identity` reads `/proc/<pid>/environ` and returns the
`CCCC_GROUP_ID` / `CCCC_ACTOR_ID` the process actually carries.
`Session::verify_process_identity` compares that pair with the session's own;
`write_locked` calls it before every write and returns
`RuntimeError::IdentityMismatch` instead of delivering.

- Fails closed: no bytes are written to a terminal running as another actor.
- Does not over-reach: when the environment cannot be read or does not carry
  both variables (runtimes that clear their env), the check is skipped and
  delivery proceeds as before.
- Surfaces once rather than looping: on this build the daemon's rejection path
  records one error per event and opens the per-actor breaker (T772).

Regression tests (`crates/cccc-runtime/src/manager_tests.rs`) launch a real
pty that re-execs as another identity and assert the delivery is refused with
`IdentityMismatch`, plus a positive control that the same terminal under its
own id still accepts.

## Follow-ups

- Log `(delivery_id, actor_id, session pid, started_at)` at claim time so a
  future incident is reconstructible from the daemon side alone.
- A `cccc doctor` sweep over registered sessions reporting any slot whose
  process identity differs from its key.
- The seat-side fan-out tooling that renders one bootstrap per wave is outside
  this repository and still needs its own card.
