# Usage report fixture provenance

`usage_report_captured.json` is a bounded privacy projection of ten real ledger
records: five settled usage records and five task-related context-sync records.
Prompt and message content was omitted, group and actor identities were replaced,
and event, turn and generation identifiers were consistently replaced. The
provider model, token counts, times, operation names and original ordering are
retained. No captured turn contains a task reference. Tests that add links,
transitions, change times or prices label them as mutations or illustrative
operator input; those controls are not evidence that a captured turn was linked.

`context/captured_tasks.json` is a metadata-only projection of three real done
tasks. Task numbers and assignee are consistently replaced; titles, notes and
other content are omitted. Tests that move a task change its status as a labeled
mutation. Neither fixture establishes complete ledger or lifecycle coverage.
