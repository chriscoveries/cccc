# Settled turn usage

Managed Pi and Claude Actors append one `turn.usage` event to their Group ledger
when the owned turn settles. A failure or cancellation can still have reported
usage. Partial output and tool completion do not settle a turn.

Pi counters come from assistant `message_end.message.usage` snapshots across the
whole turn, including messages before tool calls. Claude counters come from
assistant `message.usage` snapshots. Claude can repeat one provider message in
several content records: the latest snapshot for each message ID is counted once,
then distinct messages are summed at settlement.

Each event names the Actor, provider model, generation and turn ID, source,
start/end timestamps, input/output tokens, cache-read/cache-write tokens, and the
number of usage snapshots.

Two task fields are reported, and they are not the same thing. `task_id` is the
provider's own task or turn authority, projected as `effective_active_task_id`,
read before settlement clears it; absent authority stays null. `task_ref` is the
board task the delivered batch named in its `task_refs`, captured when that batch
was admitted, and it is the only field a turn's cost is charged to. The card is
held on the turn itself from the moment the runtime reports that turn started, so
a delivery that arrives while a turn runs — and is deferred, or refused — cannot
rename what that turn is charged to; a delivery that starts no turn drops its
card. It stays null
unless the delivery named exactly one task: refs are counted, not deduplicated,
so two refs that happen to name the same task leave it null just as two different
tasks do, and a `task_ref` carrying no id counts as a ref but names nothing. A provider token is never read as a board task, and assigned cards are
never used to guess attribution. Missing counters,
overflowing sums or mixed/unknown models stay null. Prompts, replies, tool
arguments and opaque provider payloads are not copied into usage events.

An interrupted append is retained durably and retried by the existing Actor
activity publisher, including after daemon restart. Per-generation turn receipts
prevent a duplicate completion or retry from charging the same turn again.

Read recent events with `cccc usage --group <group> --actor <actor> --limit 50`.
The Actor filter is optional. The daemon's read-only `turn_usage` operation accepts
`group_id`, optional `actor_id`, and `limit` (1–1000). This records counters only;
it does not price, budget, dispatch, stop or otherwise control work.

Codex, OpenCode and Kilo usage remains **pending capture**. No counters are inferred
for those runtimes or for unmanaged terminal sessions. Standalone Voice sessions
without an Actor/Group identity have no Group ledger attribution.
