# Browser delivery reset capture

The target is a minimal projection of a real persisted browser delivery captured
on 11 November 2026: submitted, zero reconcile attempts and an empty error.
Actor and event/turn/delivery identifiers are redacted; conversation URLs,
bootstrap material and browser inventories are omitted. Timestamps and state
values are unchanged. No user message body is included.

The capture's accompanying trace recorded a completed handoff, but redaction did
not preserve enough identifier correlation to prove the exact receipt join.
Tests therefore retain that distinction: the captured target alone cannot
authorize reset. Positive cases explicitly install an exact committed receipt
through the native completion writer as a consumer-controlled test mutation.
They do not claim the capture itself proves that join or provider completion.
Other controls mutate only identity, lifecycle, route and failure boundaries.
