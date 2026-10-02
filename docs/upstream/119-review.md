# Post-merge review — ChesterRa/cccc#119 (viewer detach / managed-session lifecycle)

Date: 2026-10-02 · Reviewer: lab-devin-rt (non-author lane) · Card: T133

## Scope

waterbang's narrower #119 was merged upstream as `63367dbe` (commits
`ea659b80`, `7322a10d`, `dbd8dc4a`). This review checks whether
**upstream/main** (`c36a4e4c`, which also carries `c270e276` and the Sep-30
"harden runtime lifecycle" commit) actually fixes the viewer-exit bug we
reported: a reaped `claude attach` viewer used to kill the provider job
(SIGTERM, exit 143) within ~13–40 s of spawn.

## Harness

- Shadow daemon only: `CCCC_HOME=/var/lib/cccc-shadow`, group
  `g_984a9b639337`; live daemon untouched.
- Binary: `cargo build --release --bin cccc` at upstream `c36a4e4c`
  (sha of tree = upstream/main HEAD at test time).
- Test actor: `sh-claude`, runtime `claude`, managed_session
  (`claude --model claude-haiku-4-5 --dangerously-skip-permissions`).
- Provider jobs are supervisor-owned under the shared `claude.exe daemon
  run` (pid 20022); viewers are `claude attach <short>` children of the
  CCCC daemon.

## Results

### (a) Viewer kill → provider survives, detach, reattach — PASS

- Started `sh-claude`: session `597eff05…`, status `usable`,
  `resume_eligible=true`, viewer = `claude.real attach 597eff05` (pid
  2743622, child of shadow daemon), provider = bg job slot `5ea14824`.
- `kill -TERM 2743622` (viewer): provider job still alive at +45 s —
  pre-fix behavior would have SIGTERMed the provider. Ledger/headless
  events show `headless.session.viewer_detached` (18:44:37Z); **no**
  `actor.stop`, no provider reap.
- `cccc send … --to sh-claude` (delivery): new viewer `attach 597eff05`
  spawned (pid 2821751), `headless.session.viewer_attached` (18:45:33Z) —
  reattach to the *same* provider session.

### (b) actor stop → confirmed kill — PASS (with caveat)

- Frozen provider (`SIGSTOP` on bg-pty-host 2743222 + bg-spare job
  2743337) then `cccc actor stop sh-claude`: op accepted
  (`actor.stop` 18:47:05Z), actor `running=false`.
- `SIGCONT`: pending supervisor kill landed, job process 2743337 exited;
  viewer `attach 597eff05` gone. Confirmed kill works end-to-end.
- Caveat: a *failed* kill (supervisor reports job still live) could not be
  induced with the real shared supervisor without freezing the live box's
  claude daemon — not attempted. The retryable path is preserved in code:
  `stop_after_process_exit` keeps the confirmed-stop path and leaves
  `status=error`/ownership on failure (session.rs:38-58), and
  `kill_and_confirm` waits for job disappearance (`STOP_TIMEOUT=10s`).
  Upstream's own regression tests (`viewer_detach_tests.rs`,
  `shutdown_tests.rs`) cover the reaped-viewer/detach/reattach matrix.

### (c) Forced resume failure → no loop, but receipt NOT invalidated — PARTIAL FAIL

- Session record poisoned to a valid-format dead id
  (`deadbeef-dead-4eaf-abd1-dd20bf0cb1ee`, `resume_eligible=true`).
- `actor start` #1: failed — `io_error: Claude Agent View did not expose
  its durable transcript`. Actor stayed stopped; **no respawn loop**
  (single `headless.session.stopped` at +84 s, no retry burst in ledger).
- Receipt after failure: unchanged — `status=usable`,
  `resume_eligible=true`, `failure_count=0`, `last_resume_error=""`.
  The binding is *not* marked not-resume-eligible.
- `actor start` #2: succeeded — `captured_from=claude_agent_view_resume`,
  viewer `attach deadbeef` running. The supervisor accepted the unknown
  resume id and effectively created a *different* session under it; the
  receipt still claims the old identity.

**Root cause:** `invalidate_managed` — added by `dbd8dc4a` with the
`a_binding_from_a_provider_confirmed_gone_is_never_resumed` test — was
**deleted by `c36a4e4c`** ("feat: add Voice persona mode and harden
runtime lifecycle", Sep 30, post-merge). At upstream HEAD the function is
gone entirely (0 references), `record_managed` still writes
`resume_eligible: true` unconditionally, and nothing marks a failed
resume binding ineligible. This is exactly the gap our fork-only
`fix/managed-resume` (`6f18a260`, patches.list:10) covers: invalidate the
managed receipt on resume failure and retry fresh.

## Verdict

#119 as merged fixes the reported bug: viewer death no longer reaps the
provider (a), detach/reattach works (a), explicit stop still confirmed-
kills (b). **But upstream HEAD is weaker than the merged PR**: `c36a4e4c`
silently removed the resume-binding invalidation, so (c) only holds the
no-respawn-loop half. `fix/managed-resume` must stay in the lab patch
queue; consider offering it upstream as a follow-up to #119.

## Evidence index

- Build: `/home/agent/work/t133-upstream-main` @ `c36a4e4c`,
  `target/release/cccc`.
- Session record: `/var/lib/cccc-shadow/groups/g_984a9b639337/state/runtime_sessions/sh-claude.json`
- Headless events: `…/state/headless/events.jsonl`
  (`viewer_detached` 18:44:37Z → `viewer_attached` 18:45:33Z →
  `stopped` 18:47:05Z → `started` 18:48:06Z → `stopped` 18:49:30Z).
- Group ledger `actor.start`/`actor.stop` count: 10 total, no respawn burst.
