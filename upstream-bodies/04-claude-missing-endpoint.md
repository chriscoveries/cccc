BRANCH: chriscoveries/cccc up/claude-missing-endpoint  (base: upstream/main d1ba8424)
DUP-CHECK: no upstream issue/PR on missing-endpoint stop handling. Adjacent to our merged #133 (session disconnect) but disjoint.

=== UPSTREAM ISSUE ===
Title: stopping a Claude Agent View actor burns the full stop timeout on an already-gone endpoint

`kill_and_confirm` treats `NotFound` from the control list call as transient. A task whose harness is already gone burns the whole 10 s STOP_TIMEOUT and then fails daemon shutdown ("failed to stop managed Actors … No such file or directory"). The endpoint being persistently absent is the strongest signal the task is already stopped.

=== PR BODY ===
Hi @ChesterRa — a fix for slow, failing stops of Claude Agent View actors whose tasks are already gone.

`kill_and_confirm` treated `NotFound` from the control list call as transient, so a task whose harness was already gone burned the whole 10 s STOP_TIMEOUT and then failed the daemon shutdown ('failed to stop managed Actors … No such file or directory', exit 1). The change tracks how long the endpoint has been continuously `NotFound`; past a confirmation window the task is gone and the stop succeeds. Genuinely stuck tasks still time out.

The confirmation window is 2 s, not a single miss: Agent View jobs survive a harness restart, so a shorter window risks reading a restarting endpoint as stopped and double-relaunching it. (Widened from 500 ms during our internal review for exactly that reason.)

## How we built it

Base commit d1ba8424. Two commits in `ops/codex_voice_analyst/claude.rs`: the persistence tracking (NotFound streak vs last-seen), and the widened 2 s confirmation window. Bounded: still capped by STOP_TIMEOUT, and a single fresh sighting resets the streak.

## How we tested it

- `cargo test -p cccc-pair-daemon --lib missing_endpoint stop_` on the PR branch: 13 passed, 0 failed — a missing endpoint stops quickly (>= GONE_CONFIRM, < STOP_TIMEOUT), a still-listed task still times out.
- `cargo fmt`/`cargo clippy --all-targets -- -D warnings` on cccc-pair-daemon: clean.
- Test env: Skylake 8 vCPU, 15 GB, Debian 13. In production on firestar (build 0990769c) since 2026-10-10.
