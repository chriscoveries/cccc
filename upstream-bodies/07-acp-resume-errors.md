BRANCH: chriscoveries/cccc up/acp-resume-errors  (base: upstream/main d1ba8424)
DUP-CHECK: #131 (closed by maintainer fix) covered the binding-side recording of resume failures; this PR is the complementary operator-facing half — error classification + recovery command. #124 closed, unrelated. No dup.

=== UPSTREAM ISSUE ===
Title: a failed ACP session resume is reported as a login failure, with no recovery named

When a native-ACP actor's saved session cannot be loaded, the launch wrapper relabels the error as "ACP initialization failed; check native CLI login and launch options" — sending the operator to re-authenticate a CLI that is running fine, and never naming the recovery that works (attempted sessions are never replaced automatically, by design).

=== PR BODY ===
Hi @ChesterRa — small correctness fix on the ACP resume error path, plus the recovery command it points at.

When a native-ACP actor's saved session cannot be loaded, the launch wrapper relabelled the error as "ACP initialization failed; check native CLI login and launch options". That sends the operator to re-authenticate a CLI which is running fine, and it never names the recovery that actually works, because attempted sessions are never replaced automatically.

- A failed session/load now keeps its own diagnosis, names the session that would not load, and points at the new-session recovery. The wrapper passes that through unchanged instead of re-wrapping it.
- Adds `cccc actor new-session <actor>`: dispatches the existing `actor_new_session` operation through the same lifecycle helper the other actor commands use, so the recovery the error names is actually reachable from the CLI.

(Complements #131, which fixed the binding side — the session receipt no longer keeps a stale `resume_eligible`. This is the operator-facing half: the error is classified correctly and the recovery is named and invocable.)

## How we built it

Base commit d1ba8424. Two commits: the resume-vs-init error split in `native_acp.rs`/`launch_native_acp.rs` with a regression test, and the thin `new-session` CLI wire-up in `cccc-cli`. The deliberate minimalism — reuse the existing op, no new daemon surface — is the shape we wanted.

## How we tested it

- `cargo test -p cccc-pair-daemon --lib resume native_acp` : 40 passed, 0 failed — including `failed_resume_names_the_recovery_instead_of_blaming_login`.
- `cargo fmt`/`cargo clippy --all-targets -- -D warnings` on cccc-pair-daemon: clean.
- Test env: Skylake 8 vCPU, 15 GB, Debian 13. In production on firestar (build 0990769c) since 2026-10-10 — used live when a Kilo lane's session died and `new-session` was the documented recovery.
