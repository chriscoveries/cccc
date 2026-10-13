BRANCH: chriscoveries/cccc up/restart-backoff  (base: upstream/main d1ba8424)
DUP-CHECK: open upstream #128 is the same failure shape (resume retry storm) — this PR bounds it; noted in body. Merged #135 handled unaddressable-scope terminal delivery; this adds restart-loop backoff + the daemon-respawn ledger event. Complementary, not a dup.

=== UPSTREAM ISSUE ===
Title: automatic actor restarts have no backoff; undeliverable jobs retry forever

When a stopped actor's process exits again soon after an automatic restart, a busy group restarts it on every message — the same crash loop upstream issue #128 reports (~171 jobs in 8.5 minutes). Separately, a startup-respawn actor gets no `actor.start` ledger event, so it reads "stopped" while actually running.

=== PR BODY ===
Hi @ChesterRa — this one bounds the restart loop behind #128 and fixes a ledger gap we found while chasing it.

**Backoff for automatic restarts.** The delivery worker starts a stopped actor when a message arrives for it; if that process exits again within `RESPAWN_WINDOW` (60 s), the restarts form one loop and each wait grows 10 s → 20 s → 40 s → 60 s (`actor_respawn_backoff.rs`). A start, stop, restart, or removal by a person — or a daemon restart — clears the history, so an operator's explicit action is never delayed. Custom-batch restart waits are interruptible and do not launch.

**Undeliverable messages terminate instead of retrying forever.** `terminal_reason` classifications are applied where the worker used to keep the claim and retry; terminal deliveries settle failed and release the claim.

**Startup respawn now journals `actor.start`.** `restore_group` relaunched actors via `actor_runtime::apply` with kind `actor.restore`, which never appended to the ledger — a fleet of respawned actors read "stopped" while running. A successful restore now appends `actor.start` with `reason=daemon_respawn`, matching the `lifecycle()` pattern. A path-level test pins the call site: removing it fails the suite (the earlier helper-level test could not).

## How we built it

Base commit d1ba8424. Three commits: the backoff module + worker termination changes (+~1,350 lines incl. ~1,030 lines of new tests), the daemon-respawn ledger event, and the call-site pinning test. The backoff state is in-memory by design — a daemon restart resets it, which is the correct semantics for "loops".

## How we tested it

- `cargo test -p cccc-pair-daemon --lib respawn` : 13 passed; `--lib backoff` : 8 passed — including `custom_batch_restart_wait_is_interruptible_and_does_not_launch`, `explicit_custom_start_and_restart_bypass_automatic_backoff`, and the restore-ledger path test.
- `cargo fmt`/`cargo clippy --all-targets -- -D warnings` on cccc-pair-daemon: clean.
- Test env: Skylake 8 vCPU, 15 GB, Debian 13. In production on firestar (build 0990769c) since 2026-10-10; in that time we have seen zero restart storms.

Scope note: #128's remaining half — falling back to a fresh session when resume comes back under a new session id — is the continuity work; we have that as a separate candidate PR rather than folded in here.
