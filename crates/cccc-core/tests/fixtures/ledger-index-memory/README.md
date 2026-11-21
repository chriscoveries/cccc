# Retained ledger replay

`captured.jsonl.gz` contains 94,648 retained events captured on 2026-10-13.
The original capture contained 97,123,335 bytes. Redaction produces 81,730,195
bytes (maximum line 16,025 bytes). `sample.jsonl` selects three events from it.

Every identity and payload string is replaced by a keyed digest repeated to
the original UTF-8 byte length. Non-index payload field names become ordinal
field names. Event kinds, timestamps, JSON types, object cardinality, record
order and relation equality remain. Original user text, identities, tool
inventories and credentials are absent. The workload measures retained shapes
and sizes, rather than application meaning of redacted payload fields.

Decoded SHA256: `5bcf36cdf5053a6016ba573ec6e4ac9143ce1cd067f68a2eb89f64736646d521`.
Gzip SHA256: `89b5c0c5081b750bc3d0b2c89478dc646fa07f32507c0d26e90cd93aa2213e21`.

CI runs the same replay executable in fresh processes against the pinned
baseline and candidate. It checks result/order fingerprints, then reports
elapsed times, steady RSS, query peak RSS and peak including temporary bulk
views. Linux `/proc` RSS includes allocator retention; it is not an allocation
profile. Fixture preparation happens outside those measured processes.
