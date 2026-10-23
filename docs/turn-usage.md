# Settled Pi turn usage

Managed Pi Actors append one `turn.usage` event to their Group ledger when the
existing RPC adapter recognizes settlement of an owned turn. Failed and cancelled
turns retain any reported usage. Output, tool completion, lower-level `turn_end`
and process lifetime do not establish settlement.

Pi supplies assistant `message_end.message.usage` snapshots. Distinct assistant
messages across tool calls are summed once; repeated snapshots replace the same
message, identified by `responseId`, then its timestamp, with the adapter's message
sequence as a fallback. Replayed completion notifications share a durable receipt
keyed by Group, Actor, runtime generation and turn ID.

The flat event data contains `actor_id`, `turn_id`, `generation`, `status`
(`completed`, `failed`, or `cancelled`), `source` (`pi_rpc`), `model_id`,
`started_at`, `ended_at`, `input_tokens`, `output_tokens`, `cache_read_tokens`,
`cache_write_tokens`, and `usage_samples` (distinct ended assistant snapshots).
Start/end timestamps are CCCC observation times, not provider billing times.
`model_id` preserves Pi's assistant `message.model` only when every snapshot
reports the same nonempty value; unknown or mixed models remain null.

Counters preserve Pi's `input`, `output`, `cacheRead` and `cacheWrite` categories
respectively. They are observations of the runtime's reported categories, without
pricing or conversion into a bill. A category stays null if any assistant snapshot
omits it or its sum overflows; reported zero stays zero. Messages with no usage
object also leave the categories unknown. Prompts, replies, tool arguments and
opaque provider payloads are excluded from the record.

Before appending, the writer persists a pending event under the Group state
directory. The existing Actor activity publisher retries pending events after an
append failure or daemon restart. If a crash interrupted the receipt write, it
checks the ledger (including rotated segments) before appending again. This
recovers settled observations already staged for writing, not unfinished turns
whose settlement was never observed.

Read these records through existing ledger tail/search access with `kind=all`;
`ledger_search` can find `turn.usage` or `pi_rpc` and filter by Actor using `by`.
In Web, select All in ledger search to see the same fields, with unknown values
shown explicitly. There is no separate usage query API or CLI. Standalone Pi
sessions without a Group/Actor and other runtimes produce no usage records.
