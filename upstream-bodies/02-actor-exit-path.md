BRANCH: chriscoveries/cccc up/actor-exit-path  (base: upstream/main d1ba8424)
DUP-CHECK: upstream #139 (closed) discusses worktree accumulation after exits, not exit recording; PRs #84/#119/#133 are unrelated. No dup.

=== UPSTREAM ISSUE ===
Title: actor ledger cannot tell a SIGKILL from an ordinary exit(1)

portable-pty reports a signal death as exit code 1, so `actor.stop` records "Exited with code 1" for a killed process exactly as for a clean nonzero exit — and nothing records which path observed the exit (the runtime wait vs the managed-session reader teardown). Reaping and crash analysis end up reading `reason` alone.

=== PR BODY ===
Hi @ChesterRa — a small observability fix in the actor runtime.

portable-pty reports a signal death as exit code 1, so a SIGKILL and an ordinary `exit(1)` were identical in the ledger. The change keeps the `ExitStatus` rendering ("Exited with code 1", "Terminated by Killed") alongside the numeric code and journals it on `actor.stop` as `exit_detail`. Each process-exit event also gets a `source` tag: `runtime` for an observed process exit, `managed_reader` for the managed-session reader teardown, which records no exit code.

Both fields are additive; existing readers of `reason` and `exit_code` are unchanged.

## How we built it

Base commit d1ba8424. One commit touching two files: `ops/actor_runtime/reconcile.rs` (journal `exit_detail` + `source` on the exit record) and `cccc-runtime/src/session.rs` (carry the rendered status). Chose additive fields over changing `reason` so existing ledger consumers keep working.

## How we tested it

- `cargo test -p cccc-pair-daemon --lib reconcile` on the PR branch: 7 passed, 0 failed.
- `cargo fmt -p cccc-pair-daemon --check` and `cargo clippy -p cccc-pair-daemon --all-targets -- -D warnings`: clean.
- Test env: Skylake 8 vCPU, 15 GB, Debian 13. In production on firestar (build 0990769c) since 2026-10-10 — this is how we distinguished a killed lane from a clean exit during a staged soak.
