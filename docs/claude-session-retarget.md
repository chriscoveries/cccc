# Resume a saved Claude conversation

A managed Claude actor can resume a specific saved conversation:

```sh
cccc actor resume-session claude-1 <session-uuid> --group <group-id>
```

The actor controls in the web UI offer **Resume session…** for Claude actors. Enter
its full session UUID. This explicit action uses the same authorization as
Restart and starts the actor after selecting the conversation.

The daemon operation is `actor_resume_session`, with `group_id`, `actor_id`,
`session_id`, and the usual `by` argument. The web endpoint is
`POST /api/v1/groups/{group_id}/actors/{actor_id}/resume_session`, with a JSON body
containing `session_id`.

The actor must have a managed Claude Agent View receipt, and automatic runtime
resume must be enabled. Before changing the actor or its receipt, CCCC verifies:

- The transcript is a regular, contained file in the actor's Claude account's
  project directories. Claude's project slug includes its long-path hash suffix.
- The launch workspace matches the transcript's residence project slug, its latest
  recorded cwd, or the same conversation's native job worktreePath. Historical cwd
  records from before EnterWorktree do not veto a matching current location.
  A transcript outside the launch project must have unique residence in this account;
  ambiguous, mismatched-session, oversized, and symlinked transcripts are refused.
- The existing receipt identity fingerprint matches the resolved actor settings (model-only drift is allowed),
  including its private environment and config-directory selection.
- Agent View reports no live job for the requested conversation, and Claude's
  session registry reports no live process holding it. CCCC reports the job ID or
  process PID it can identify. Close a holder through its owner before retrying.
  This also applies if the requested conversation is already running in this actor.

A refusal preserves the actor configuration and receipt. CCCC never kills a
foreign holder. The live check is repeated during launch, and explicit retargeting
never adopts a live job that appeared after validation. Claude's existing exact-ID
check still rejects a copied conversation.

Resume launches contain exactly `--bg --resume <session-uuid>`. Passing `--model`
to that launch forks the conversation. Explicit selection also skips the normal
continuity model-change prompt: it restores the conversation with its retained
model. The operation result and receipt include `model_application`, reporting
`configured_model_applied: false`, the configured model, and the latest assistant
model recorded in the selected transcript (or null when unknown). The receipt's
`model` describes that retained model, and is empty when unknown. Apply actor model
settings separately through ordinary Start/Restart; the continuity foundation
uses its owned-session model prompt and confirms persisted provider state.

On success, the replaced target is retained in the receipt's optional
`previous_sessions` array. Each entry contains `session_id`, `workspace_path`,
`model`, `replaced_at`, and `reason`. The array retains up to 20 distinct targets;
repeating the same selection does not duplicate an archive entry. Older receipts
without the field remain usable. Successful ordinary Stop/Start cycles preserve
this archive.

If launch or lifecycle persistence fails, CCCC stops the replacement, restores the
previous receipt fields, retains both targets in the archive, and reports the
failure. It does not attempt a fresh conversation or automatically restart the
previous target as part of rollback. The actor remains stopped for an explicit
operator decision.

Identity validation uses the existing receipt fingerprint and the selected config
directory. Claude transcripts do not carry a verifiable account fingerprint; this
operation does not import transcripts across accounts or recover sessions whose
original actor identity can no longer be verified. Process-registry inspection is
conservative: unreadable live metadata prevents selection, and a reused live PID
can require the holder's stale metadata to be resolved by its owner. The provider
can still race a concurrent external launch; a copied result is rejected and its
newly started job is cleaned up by the existing launcher.

For a disposable real-CLI reproduction, run
`scripts/repro_claude_resume_session.sh` from the repository root. It creates
separate CCCC and Claude homes, uses Haiku, remembers a random token in conversation
A, injects a throwaway receipt B, changes the configured actor model to Opus,
resumes A with its original Haiku model, and verifies bare resume arguments,
its answer and B's archive
entry. It copies only authentication/account metadata into the scratch config,
never existing transcripts or job state. Override `CCCC_REPRO_CLAUDE` to select a
direct Claude executable and `CCCC_REPRO_AUTH_DIR` to select an authentication
source. `CCCC_REPRO_BINARY` selects CCCC and `CCCC_REPRO_EVIDENCE` selects the output
directory. A stock CLI produces command-absent evidence without starting a daemon.
All processes started in the scratch environment are stopped and its homes removed.
