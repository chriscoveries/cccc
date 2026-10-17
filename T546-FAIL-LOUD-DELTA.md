# T546 fail-loud slice — native terminal delivery confirmation

Branch: fix/native-delivery-fail-loud (worktree /home/agent/work/hermes3-t546-fail-loud)
Base: 41908bbfc44f14605e1f7ef59e405b5de69246cf (origin/main)
Author lane: lab-hermes3, card T546, 2026-10-17. Source-reviewer: non-author pending.

## What changes (production)

crates/cccc-daemon/src/ops/actor_delivery_worker.rs
crates/cccc-daemon/src/ops/actor_delivery.rs

1. `wait_for_input_mode` returns a typed `InputModeOutcome {Ready, NotRunning,
   Cancelled, TimedOut}` instead of bool. The old tail `!cancelled.load(..)`
   returned TRUE after INPUT_MODE_TIMEOUT — an elapsed wait certified an
   assumed-ready input. TimedOut no longer certifies anything.
2. Native batch submit goes through `submit_terminal_text_checked`, which
   returns `SubmissionOutcome {Confirmed, Unconfirmed{reason}, Failed}`:
   - Failed (nothing written) -> Retry, unchanged.
   - Unconfirmed: empty submit sequence (ActorSubmit::None writes no Enter —
     unconfirmable by construction), or input mode TimedOut on first delivery.
     The payload is still written (best effort, same bytes as before).
   - Confirmed: payload + at least one submit key written, input confirmed.
3. Unconfirmed batches settle via `unconfirmed_jobs`, a mirror of the managed
   path's existing `BatchSubmission::Unconfirmed` arm: ledger state
   `ambiguous` (reason string includes bytes written), in-flight released,
   no automatic retry. The sender sees uncertainty instead of `accepted`.
4. `submit_terminal_text` (bool) stays for non-delivery callers
   (local_headless supervisor, Custom preamble): it delegates to the checked
   form with input_confirmed=true and maps only Failed to false — byte-for-byte
   the old write verdict.

## Unchanged invariants

- Write path: same payload construction (bracketed paste / flattened lines),
  same submit sequence, same delays. Unconfirmed still writes; nothing is
  withheld, so no new loss mode versus origin/main.
- Retry semantics: Failed -> Retry exactly as before; only the settlement of
  a SUCCESSFULLY-WRITTEN-but-unprovable batch changed (accepted -> ambiguous).
- Managed (local_headless), Deepseek, Custom preamble, and Codex-voice paths:
  untouched call graphs; the managed path already had Unconfirmed.
- `append_state` / runtime.delivery event schema: untouched (15 callers stay
  valid). The byte count rides in the existing `reason` string field.
- Delivered batches still release in-flight and advance the preamble session
  (including Unconfirmed — bytes were written, so the preamble is not repeated).

## Tests (all new, additive)

actor_delivery.rs: zero-submit actor grades Unconfirmed; unconfirmed input mode
grades Unconfirmed; confirmed submit key stays Confirmed and the plain write
verdict is unchanged. Live-PTY fixture `stty raw -echo; touch ready; sleep 30`
— a TUI that never acknowledges paste, the cline-a shape.

actor_delivery_startup_tests.rs: wait_for_input_mode reports TimedOut (was
true) and Cancelled; an unconfirmed terminal delivery settles `ambiguous`
(latest_state) instead of `accepted`.

## Expected effects (source reasoning only)

- claw cline-a's false-accept shape becomes visible: a notice written to a
  lane that never submits settles `ambiguous` in the ledger, and the reason
  names whether the submit key or the input mode was missing.
- No automatic retry is introduced for unconfirmed writes (matches the
  managed-path precedent: human inspects before explicit retry).

## Residuals / limits

- The stage reproduction (card step 1) remains gated on T543; this slice is
  the fail-loud design (steps 2-3) implemented ahead of it, per the card's
  own note that steps 2-3 are not gated.
- Suspect (b) `submit:"none"` as an actual claw cline deployment config is
  still unverified (deployment bytes, not source); the slice is robust to
  either suspect because it grades by what was written, not which suspect fired.
- Follow-up not in this slice: first-delivery grading only; subsequent
  deliveries to a live session keep the origin/main confirmation model except
  for the structural zero-submit case (config-true on every delivery).
