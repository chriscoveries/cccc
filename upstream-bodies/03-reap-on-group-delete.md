BRANCH: chriscoveries/cccc up/reap-on-group-delete  (base: upstream/main d1ba8424)
DUP-CHECK: upstream PR #118 (closed, consolidated defect fixes) and #102 (closed) do not cover detached-child reaping on group delete. No dup.

=== UPSTREAM ISSUE ===
Title: detached child processes survive `group delete` with the group's env intact

A session stop kills the process tree the runtime owns, but a child that detached into its own session (new session leader, or a provider CLI daemonising itself) leaves that tree and survives — still carrying the group's CCCC_GROUP_ID. `group delete` then reports success while the deleted group's processes keep running.

=== PR BODY ===
Hi @ChesterRa — this closes a leak we reproduced on the deployed 0.4.41 build.

A session stop kills the process tree the runtime owns; a child that detached into its own session (a new session leader, or a provider CLI daemonising itself) leaves that tree and survives it, while still carrying the group's `CCCC_GROUP_ID`. Group delete then reports success with the deleted group's processes still running.

Reproduced on a throwaway home with 0.4.41: a detached `sleep` started by an Actor survived `group delete` (reparented to init, CCCC_GROUP_ID intact), while a plain child and a child that stayed in the session's process group died with the session.

Delete now sweeps /proc for same-user processes still carrying the deleted group's `CCCC_GROUP_ID` and stops them (SIGTERM, then SIGKILL after a grace), reporting them in `reaped_processes`. Linux only: there is no portable process-environment scan elsewhere, and the sweep never signals a process it cannot read and does not own.

## How we built it

Base commit d1ba8424. One commit: new `ops/actor_runtime/attributed_reap.rs` (the /proc scan, attribution check, and TERM→KILL grace), a call site in `ops/groups.rs::delete`, and a `procfs` dependency. The sweep deliberately checks uid + env ownership before signalling so it can never hit another user's or group's processes.

## How we tested it

- `cargo test -p cccc-pair-daemon --lib reap` on the PR branch: 6 passed, 0 failed — covers detached-child reaping and the leaves-other-groups-alone guarantee.
- `cargo fmt -p cccc-pair-daemon --check` and `cargo clippy -p cccc-pair-daemon --all-targets -- -D warnings`: clean.
- Test env: Skylake 8 vCPU, 15 GB, Debian 13. In production on firestar (build 0990769c) since 2026-10-10.
