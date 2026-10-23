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
board task the delivered batch named in its `task_refs`, held for the length of that
batch's admission, and it is the only field a turn's cost is charged to. The card is
claimed by whichever comes first — the runtime's acceptance of the batch, its
`turn/started`, or its completion — because any of the three can be observed before
the others. When acceptance comes first the card is bound to the turn id the runtime
named and looked up by that id when the turn starts; when the start event comes
first it claims the card itself. Deliveries are admitted one at a time per Actor, so
the card being claimed always belongs to the delivery being resolved: a second
delivery — deferred, refused, or merely observed first — cannot rename what the
running turn is charged to. A batch that named no single card still gets an entry,
holding nothing, so a turn resolved as unattributed never falls back to another
delivery's card; a delivery that starts no turn leaves no entry at all, and a
terminal route with no authoritative turn id leaves the turn unattributed rather
than guessing. It stays null
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
