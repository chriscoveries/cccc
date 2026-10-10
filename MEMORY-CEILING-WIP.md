# Memory ceiling handover

Stopped per owner order. No production implementation exists.

Branch: wip/actor-memory-ceiling-sol. Base: local lab 0990769cb29b7f19e097cec94542abc3b25f8a2e (origin/lab fetched d067059f is older).
Worktree: /home/agent/work/sol-actor-memory-ceiling.

Two new daemon tests exercise default-off serialization, configured round-trip, and rejection of negative/fractional/string/overflow limits. They should fail on this base, which has no Actor.memory_limit_mb. The claw run was cancelled before any verified result; /tmp/sol-memory-settings-red.log is incomplete, not evidence.

Actor is defined in crates/cccc-contracts/src/actor.rs:170. Core actors::update serializes the Actor, merges patch, then deserializes; daemon actor_from_args supports actor object and flat args. A serde-default u32 with skip-zero retains default-off serialization and rejects the tested invalid values. No field added yet.

Existing scheduler runs each group under group_write in server_automation.rs. automation_runtime::tick_group is the periodic hook. local_headless::ready_for_mail_notice checks running, active_turn absent and status idle; it is a snapshot, not a restart fence. HeadlessStatus.pid can represent the viewer, so do not assume it identifies the provider tree. Need ownership-verified provider roots and PID birth identity, with idle/ownership rechecked at restart admission. PTY heuristic idle cannot prove never-mid-turn.

actor_runtime::apply(home,group,actor_id,"actor.restart") stops registered ownership and starts using runtime_session receipt. Require verified resumability before stopping; do not silently fresh-start if receipt absent or incompatible. Keep cooldown across daemon restarts, reserve before attempting restart, and preserve failure outcomes. One ledger event plus targeted foreman notice; no new polling loop required. Linux/macOS process-tree sampling and cooldown/idle tests remain unwritten.

No live changes, push upstream, install or successful test claim.
