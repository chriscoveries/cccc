# Observed task usage

`cccc usage --group <group-id> --task <task-id> --json` reports observed,
explicitly linked settled turns for one task. `--by-model` instead groups
observed task participations by provider model and the runtime saved with the
owning session generation. The same task can appear in several groups.

This is a bounded, read-only report over the existing ledger and task snapshot.
It never changes tasks or installs a collector. A missing old lock or a legacy
context that needs migration leaves the task snapshot unknown. Records before
this reporting feature may not contain task-transition data, and unsupported or
unattributed turns remain unknown. Context updates and event append are not one
atomic operation: coverage, claim-to-done totals and cost totals remain partial
or unknown. A selected page containing legacy task changes without transition
metadata or a task-subtree deletion cannot prove interval membership. A
present-day task assignee or model label is never used to infer
historical attribution. The optional operator-declared model label is captured
for the owned session generation at turn start; it is separate from the
provider-reported model. Old or unavailable labels and runtimes are null.

Supply `--prices <file>` to price observed, linked turns. The local JSON file
contains `version`, uppercase three-letter `currency`, `minor_scale` (0–6), and
`entries`. Each entry contains an exact `model`, UTC `effective_from`, optional
`effective_until`, and decimal-string `input`, `output`, `cache_read` and
`cache_write` rates in currency units per million tokens. Intervals for a model
must not overlap. Rates cannot use exponents, signs, floating-point JSON or more
than nine fractional digits. Missing prices or counters stay unknown. Subtotals
use integer arithmetic, and the final minor-unit amount rounds half to even.
The table hash and version identify the local price input; estimates are not
invoices. Prices are not uploaded to the daemon.

Model/runtime groups report fully priced and unpriced currently-done task
counts separately. Their priced-done mean uses only qualifying turns for those
done tasks, with exact numerator and denominator and one half-even rounding;
open tasks do not enter that mean. When a done task is incomplete, the mean
over all done tasks remains unknown. These are observed estimates within the
report's stated coverage, not proof that all task usage was recorded.

The `canonical_ledger_page_sha256` names the bounded decoded page: each Event is
serialized as compact JSON in ledger order, prefixed by its eight-byte unsigned
big-endian length, and the concatenation is SHA-256 hashed. It is a snapshot
identity, not a hash of the original on-disk JSONL bytes. A missing ledger lock,
a gap, truncated changes or a missing task snapshot is recorded as incomplete.
