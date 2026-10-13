BRANCH: chriscoveries/cccc up/claude-continuity  (base: upstream/main d1ba8424)
DUP-CHECK: open #128 (resume-under-new-id retried forever) — this PR supplies the explicit-resume/fallback machinery it lacks; our restart-backoff PR bounds the retry side. No dup; cross-referenced.

=== UPSTREAM ISSUE ===
Title: no way to resume a specific saved Claude conversation; transcript races can kill a healthy session

There is no operator path to resume a chosen saved conversation for a managed Claude actor, and three smaller transcript-handling gaps: a mismatched published transcript logs unidentifiable diagnostics; interruptions are not correlated to the active assistant parent; and a transcript path published before its file exists causes an immediate disconnect instead of bounded grace.

=== PR BODY ===
Hi @ChesterRa — the largest of this batch: explicit Claude session resume/retarget plus three transcript robustness fixes. All of it has run in our production fleet for weeks.

**Explicit resume.** `cccc actor resume-session <actor> <session-uuid>` (and the matching web control, `POST .../resume_session`) retargets a managed Claude actor to a chosen saved conversation. Before changing the actor or its receipt it verifies: the transcript is a regular contained file in the account's project dirs; the launch workspace matches the transcript's residence/cwd/worktreePath; the receipt identity fingerprint matches (model-only drift allowed); and no live job or process holds the requested conversation — a refusal preserves configuration and never kills a foreign holder, with the live check repeated during launch. New modules: `claude/retarget.rs`, `claude/control.rs`, `claude/model_switch.rs`, `claude/cache_confirmation.rs`, `runtime_session/claude/{continuity,retarget}.rs`, plus a documented runbook (`docs/claude-session-retarget.md`) and a repro script.

**Transcript robustness.** (a) A mismatched published transcript now logs expected session UUID, observed filename, and job id — quoted, no directory paths — while keeping the InvalidData rejection. (b) Interruptions correlate by active assistant parent instead of a bare latest-turn guess. (c) A transcript path published before its file exists now gets the same bounded 10 s missing-file grace as an adopted path, instead of an immediate disconnect; permanent absence still fails.

## How we built it

Base commit d1ba8424. Four commits (~3,300 lines, roughly half tests + docs): the resume/retarget feature, then the three transcript fixes on top. The retarget validation is deliberately pessimistic — refuse on ambiguity, never signal a process we don't own — because the failure mode it guards against is killing someone else's Claude session. Web UI gains one control ("Resume session…", same authorization as Restart) and i18n strings for en/ja/zh.

## How we tested it

- `cargo test -p cccc-pair-daemon --lib claude` on the PR branch: 123 passed, 0 failed — retarget validation, archive idempotence, receipt fingerprint checks, delayed-publication grace, interruption correlation, and the managed-resume suite.
- `cargo fmt`/`cargo clippy --all-targets -- -D warnings` on cccc-pair-daemon: clean.
- Test env: Skylake 8 vCPU, 15 GB, Debian 13. In production on firestar (build 0990769c) since 2026-10-10, running dozens of managed Claude actors daily — this is the machinery that lets us recover a lane onto a specific prior conversation.

Related: together with the restart-backoff PR this addresses most of #128 — backoff bounds the retry storm, and this supplies the "fresh-session fallback"/explicit-resume path it identified as missing.
