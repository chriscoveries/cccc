Six projected ledger records retain three real request-reply messages and
three corresponding cancellations. IDs, timestamps, kinds, source references
and message modes are retained. Group and actor names are aliased; bodies,
paths, titles, environments, references and credentials are omitted.

This is local lookup evidence, not a Connect exchange or a claim about remote
peer authorization. Tests replay the lookup shape shared by the two callers;
their existing authorization and provenance checks remain before the lookup.
The final append is an explicit duplicate-replay lock-lifetime control.
