# Browser reset captures

`submitted.json` is a minimal saved browser projection captured on 2026-11-11.
Its original redaction removed ID correlation; it remains an unknown/refusal
control. An empty error and `submitted` alone prove no provider outcome.

`correlated.json` is a separate metadata-only capture read on 2026-11-12 from a
bounded historical query for the 2026-11-11 02:09 delivery. It retains one
addressed source, the browser submission tuple, its accepted transport record,
and the matching native completion tuple. Actor, group and event identities
are consistently redacted; message bodies and credentials are omitted. This
does not reconstruct the erased IDs in `submitted.json`.

Tests rebase these identities into temporary groups and regenerate only the
native deterministic transport/receipt IDs. Removing the completion, making a
source ambiguous, failed setup, crashes and successor bindings are explicitly
controlled mutations. No test claims a provider execution was cancelled or
that an unseen message can safely be replayed.
