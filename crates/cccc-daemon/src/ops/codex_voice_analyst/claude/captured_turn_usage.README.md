# Claude turn usage capture

Captured on 2026-10-22 from an actual Claude Code transcript. The fixture retains
14 assistant metadata records and their real `turn_duration` marker. They cover
seven provider message IDs: content snapshots repeat each message's counters.
The expected sum is 14 input, 4698 output, 1521929 cache-read and 9321 cache-write
tokens. No usage value, model, timestamp or retained identifier was invented.

Redaction is a whitelist: record type/session ID/UUID/timestamp, message ID/model,
the four token counters, and the terminal marker's subtype. All message content,
prompts, paths, environment, tool data and other fields are omitted. The replay
harness establishes the turn boundary directly because no prompt content is
retained. It uses the real assistant and settlement records unchanged.
