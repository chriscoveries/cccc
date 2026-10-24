# Initial native-turn evidence
Run in CI: `cargo test -p cccc-pair-daemon --test initial_native_turn -- --nocapture`.
The helper is test-only; no daemon behavior, queue rule or readiness timing changes.
It reports transport, observed source-bearing input, assistant consumption and backend outcome separately.
These four October 17 captures used actual OpenCode 1.19.0 and Kilo 7.8.8 with a **scripted loopback provider**.
Both early arms accepted transport but captured no native input or provider work. Both rendered controls executed fixture turns.
These are historical classifier tests, not a current-source runtime reproduction or real inference proof.

`captures.json` retains native IDs, ordered relationships, timestamps, part types/statuses and text SHA-256/byte counts.
Source markers are extracted only from exact event-ID brackets; startup-header occurrences are counted before omitting text.
Prompts, paths, tool inputs/outputs, configuration and identity fields are omitted. No records or timestamps are invented.
The cross-arm negative control selects genuine other captures. It does not alter their IDs or roles.
The two controls contain one user each; multi-user batching, error/cancellation and replay/generation controls still need captures.
The expected pre-write payload, observer/provider/viewer generation and durable admission contract were not captured.
Accordingly exact payload equality and production admission remain unknown, and full proof always remains INCONCLUSIVE.
Never hash the observed text and relabel it as an independently expected payload.

Accepted manifest: `9a9985d2560f25edbcc179705e15c3d16ef992388ea4c8025b03be18915dfee6` (29 files).
Projection: `ffeaa882abab15f4f9a644623ffd258a8ef4cc693a3a37b65adb02546a783700`.
The fixture carries original source/binary/runtime/provider-fixture and SQLite snapshot hashes for provenance review.
A real-provider cold-delivery assertion against unchanged source is a separate runtime gate, still outstanding.
That run must use normal initial delivery, without a rendered-input wait, warm-up turn or extra harness sleep.
Native-user admission and authoritative mounted-input contracts remain required before any runtime repair.
