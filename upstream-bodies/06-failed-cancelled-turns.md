BRANCH: chriscoveries/cccc up/failed-cancelled-turns  (base: upstream/main d1ba8424)
DUP-CHECK: closed #124 (Codex reconnect loop) is the same symptom family; merged #132/#133 cover a different disconnect cause. No dup.

=== UPSTREAM ISSUE ===
Title: a failed managed turn settles "idle"; a cancelled one reports "error"

A completed turn is always reported as idle, so a provider-rejected turn (e.g. an API error surfaced as an errored assistant message) leaves the session looking healthy to working_state, the actor list, and the headless journal. Symmetrically, a cancelled turn — usually a deliberate operator interrupt — would settle as error, training readers to ignore the error state.

=== PR BODY ===
Hi @ChesterRa — turn completions now get classified instead of settling idle unconditionally.

Providers signal failure out of band, so a turn rejected by the model left the session looking healthy to every watcher: working_state, the actor list, and the headless journal. The completion is now classified before settling:

- A non-completed status sets session status `error` carrying the provider's reason (surfaced as `effective_working_reason`), and emits `headless.turn.failed` with turn id, status, and error — so the failure appears in the headless journal on non-structured runtimes too.
- A cancelled (or "canceled"/"interrupted") turn settles idle and emits `headless.turn.cancelled` — visible in history without claiming breakage. Cancellation is usually deliberate; calling it an error would train every reader to ignore the error state.
- The next successful turn settles idle and clears the stale reason. The turn-id guard applies to both outcomes: a completion naming another turn is ignored either way.
- Claude and the ACP adapters (OpenCode, Kilo) publish `params.turn.status`/`.error` via their own `settle_turn`; Codex republishes the provider frame unchanged, whose error may be an object — both shapes are normalised to text. A completion reporting no status is treated as success, so an unlabelled provider response is not a false alarm.

Web side: `headless.turn.cancelled` joins the replay end types in `useSSE`, so the UI closes the turn instead of leaving it open — the raw projection happening to label a cancelled ACP turn "completed" first is an accident of ordering, not a contract.

## How we built it

Base commit d1ba8424. Three commits: the settle classification + `headless.turn.failed` event (~230 lines), the three-way cancel classification + `headless.turn.cancelled`, and the web replay fix with a regression test. Deliberately kept at the settle boundary rather than the projection layer, so all runtimes benefit uniformly.

## How we tested it

- `cargo test -p cccc-pair-daemon --lib` on `settle|cancelled|failed_completion` : 20 passed, 0 failed — including a real-session round-trip: failed turn sets error with the provider reason, next success clears both.
- Web: `vp test run src/hooks/useSSE.headlessReplay.test.tsx` : 6 passed.
- `cargo fmt`/`cargo clippy --all-targets -- -D warnings` on cccc-pair-daemon: clean.
- Test env: Skylake 8 vCPU, 15 GB, Debian 13. In production on firestar (build 0990769c) since 2026-10-10.
