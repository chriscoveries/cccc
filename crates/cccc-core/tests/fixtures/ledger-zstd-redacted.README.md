# Ledger codec capture

`ledger-zstd-redacted.jsonl` contains 64 observed ledger events, retained in
source order. Event IDs are stable one-way pseudonyms, the group ID is a
neutral constant, and scope, actor identity, message content and most data
fields are removed. The message delivery mode and recipient count are retained;
sender and recipient names are neutral placeholders. Event kinds, timestamps
and protocol versions are preserved. The fixture is a redacted projection,
not the original ledger bytes. SHA-256: `5d331c9b292b1278fa69cd443fe66c87bfe9cdf237d51199730dc260a546aa7f`.
