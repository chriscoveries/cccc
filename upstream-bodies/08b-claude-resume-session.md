BRANCH: chriscoveries/cccc up/claude-resume-session  (base: up/claude-transcript-fixes → upstream/main d1ba8424)
SUPERSEDES: second half of 08-claude-continuity.md (split per seat ruling — offered after 08a lands, or as issue first)
DUP-CHECK: supplies the fresh-session/explicit-resume half of open #128; our restart-backoff PR bounds its retry storm. No dup.

=== UPSTREAM ISSUE ===
Title: no operator path to resume a specific saved Claude conversation

A managed Claude actor can only continue whatever session its receipt last recorded. There is no way to retarget it to a chosen saved conversation — relevant when the wrong transcript was adopted, a lane must move onto a specific prior conversation, or (as in #128) a resume needs an explicit target instead of a retry loop.

=== PR BODY ===
Hi @ChesterRa — explicit session resume for managed Claude actors, plus the validation that makes it safe to offer.

`cccc actor resume-session <actor> <session-uuid>` retargets an actor to a chosen saved conversation (web control too: "Resume session…", same authorization as Restart; endpoint `POST /api/v1/groups/{group}/actors/{actor}/resume_session`).

Before changing the actor or its receipt it verifies:

- The transcript is a regular, contained file in the actor's Claude account's project directories (Claude's project slug includes its long-path hash suffix).
- The launch workspace matches the transcript's residence project slug, its latest recorded cwd, or the same conversation's native job worktreePath. Historical cwd records from before EnterWorktree do not veto a matching current location. A transcript outside the launch project must have unique residence in this account; ambiguous, mismatched-session, oversized, and symlinked transcripts are refused.
- The existing receipt identity fingerprint matches the resolved actor settings (model-only drift is allowed), including its private environment and config-directory selection.
- Agent View reports no live job for the requested conversation, and Claude's session registry reports no live process holding it — the job ID or PID found is reported. Close a holder through its owner before retrying. This applies too if the requested conversation already runs under this actor.

A refusal preserves the actor configuration and receipt. CCCC never kills a foreign holder; the live check is repeated during launch, and explicit retargeting never adopts a live job that appeared after validation. Claude's existing exact-ID check still rejects a copied conversation.

## How we built it

Base: our transcript-fixes PR on d1ba8424 (the delayed-publication grace is load-bearing for resume reliability). One commit (~3,000 lines, roughly half tests + docs): new modules `claude/retarget.rs`, `claude/control.rs`, `claude/model_switch.rs`, `claude/cache_confirmation.rs`, `runtime_session/claude/{continuity,retarget}.rs`; CLI verb; web control + en/ja/zh strings; `docs/claude-session-retarget.md` runbook and `scripts/repro_claude_resume_session.sh`. Deliberately pessimistic validation — refuse on ambiguity, never signal a process we don't own — because the failure mode guarded against is killing someone else's Claude session.

## How we tested it

- `cargo test -p cccc-pair-daemon --lib claude` : 123 passed, 0 failed — retarget validation, archive idempotence, receipt fingerprint checks, managed-resume suite.
- `cargo fmt`/`cargo clippy --all-targets -- -D warnings` on cccc-pair-daemon: clean.
- Test env: Skylake 8 vCPU, 15 GB, Debian 13. In production on firestar (build 0990769c) since 2026-10-10 — this is the machinery we use to recover a lane onto a specific prior conversation.
