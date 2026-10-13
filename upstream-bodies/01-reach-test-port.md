BRANCH: chriscoveries/cccc up/reach-test-port  (base: upstream/main d1ba8424)
DUP-CHECK: no open/closed upstream issue or PR touches this test.

=== UPSTREAM ISSUE ===
Title: flaky test: reach web-identity gate test frees its port under parallel runs

`reach_rejects_a_recorded_web_port_that_is_not_listening` binds a port, drops the listener, then probes the port it just released. The released port returns to the ephemeral pool, so a sibling fixture (same hardcoded runtime id and proof key) can take it and answer the probe with a valid proof — failing this test and stealing the single accept() the next test's live-port fixture waits on. Both tests fail together under parallel threads.

Fix: the fixture holds its own port for the whole test and closes every connection without a proof, so the probe still lands on the same gate error with nothing left to race.

=== PR BODY ===
Hi @ChesterRa — a small test-only fix for a race we kept hitting under parallel test threads.

`reach_rejects_a_recorded_web_port_that_is_not_listening` bound a port, dropped the listener, and then probed the port it had just released. That port goes back to the ephemeral pool in the gap, so another binder on the host — a sibling test's fixture, or a second instance of the suite running concurrently, whose fixtures use the same hardcoded runtime id and proof key — can take it and answer the probe with a valid proof. That fails this test, and it also steals the single accept() of the live-port fixture the next test waits on, which is how both tests failed together under parallel threads.

The fixture now holds its own port for the whole test and closes every connection without a proof: the probe still lands on the same gate error ("did not prove its runtime identity"), with nothing left to race.

The connection-refused case cannot be asserted deterministically in a shared-port test binary; `probe_identity` maps refused, reset, bad JSON, wrong runtime_id and invalid proof to the same false, so no branch is lost.

## How we built it

Base commit d1ba8424 (current upstream/main). One commit, test-only: `crates/cccc-daemon/src/ops/membership/tests/web_identity.rs` (+21/−5). The gate-under-test and the probe path are unchanged; only the fixture's port lifetime changed.

## How we tested it

- `cargo test -p cccc-pair-daemon --lib reach_` on the PR branch: 14 passed, 0 failed (includes all five web-identity gate tests).
- `cargo fmt -p cccc-pair-daemon --check`: clean. `cargo clippy -p cccc-pair-daemon --all-targets -- -D warnings`: clean.
- Test env: Skylake 8 vCPU, 15 GB, Debian 13. The same fix has run in our production fleet (firestar build 0990769c) since 2026-10-10.
