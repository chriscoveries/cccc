# Settled-turn usage

Pi RPC and Claude transcript adapters attach raw provider counters to their
existing settlement event. The owned session then appends `turn.usage` to the
group ledger. Existing ledger queries can read it; no new operation is added.

The event identifies actor, generation, turn, runtime `task_id`, nullable
`delivery_id` and board `task_ref`, settlement status, source, provider model,
input/output/cache-read/cache-write counters and observed start/end timestamps.
The runtime task token is separate from the board reference.

Only an outbound controlled batch containing exactly one valid task-ref entry
can supply `task_ref`. Duplicate, malformed, multiple or absent references stay
unattributed. The session must bind that exact delivery to the observed turn;
there is no latest-delivery or assignee inference. Native Claude terminal turns
remain usage-only unless they have that same authoritative binding.

Distinct provider messages are summed; repeated Claude message snapshots are
replaced. Missing counters, missing Claude message identity, overflow and mixed
or missing models remain unknown. Unsupported runtimes remain unmetered.
Failed and cancelled settlements retain counters already observed.

A stable group/actor/generation/turn ID and pending/committed markers protect
append retries. Recovery scans only interrupted pending writes; ordinary turns
do not build a full-history index. A crashed or unobserved turn is incomplete,
not an invented zero. Consumers must retain event-ID deduplication and report
incomplete ledger ranges. This hook does no pricing, task edits or dispatch.

Fixtures retain only usage/ref metadata from real captured turns. Provider
message identities are bijectively replaced, record/session IDs replaced, task
IDs renumbered, and prompt/content/tool fields omitted. The delivery capture's
internal model selector is replaced by `captured-model`. Numeric counters, Claude
model/timestamps, record order and duplicate relationships are preserved.
