# Ledger index memory and query costs

The query cache retains locations and lookup keys, rather than parsed events
or JSON bodies. A single-event lookup or `events_after` page decodes only the
requested bodies, including cold `actor.activity` data. Duplicate identifiers
still resolve to their last position. Event and actor-generation order remain
unchanged. Legacy absent `id`/`ts` defaults remain stable within an index.

Retained metadata has an explicit accounted budget of 128 MiB and at most 32
cached paths. Owned vector/map/string capacities are counted with a factor of
two for overhead. This is an accounting bound, not an allocator RSS limit.
Reservation follows the entry's last reader, including retired entries.
Oversized entries receive no allowance: the request can use its temporary
index, but the cache does not retain it. Parsed JSON body retention is zero,
including when a body alone exceeds the metadata budget.

On append-only growth, the index copies metadata and parses only new records.
It checks source identities, archive revisions, newline alignment and the
SHA256 of the previously observed active prefix before resuming. A rewrite,
replacement, truncation, changed segment set or unfinished line rebuilds the
metadata instead. Source errors do not publish a partial replacement.
Append notifications are no longer authoritative; canonical committed bytes
are reconciled at query time, so late or duplicated callbacks cannot duplicate
an event. Readers hold the stable shared ledger lock through hydration;
callbacks execute after releasing it. No cached entry retains a descriptor.

There are deliberate costs and limits:

- Continuity hashing still reads the old active prefix. Catch-up avoids old
  JSON parsing, rather than promising tail-only I/O.
- Metadata copying and hashing occur after ordinary notified appends too.
  This trades the previous mutable cache's fast callback for a single
  committed-byte reconciliation path. The replay reports catch-up costs.
- Warm lookups now perform body I/O and JSON decoding. Gzip lookups decode
  their compressed prefix; there is no random-access gzip index.
- `read_all`, `inspect` and `inspect_status` preserve their full-history
  interfaces. They materialize temporary full event views. Concurrent query
  results, temporary metadata, allocator arenas and process RSS are outside
  the retained-cache budget. Bulk callbacks may therefore use substantial
  transient memory even though bodies are not retained by this cache.

The retained fixture and `ledger_index_replay` example compare the pinned
baseline and candidate in separate Linux CI processes. The report includes
correctness/order hashes, cold and warm elapsed times, eight missed-notification
catch-ups, steady RSS before bulk views, query peak RSS and peak including bulk
views. RSS includes allocator retention. Unit controls separately check tail
parse counts, lazy hydration counts, bounded concurrent reservations, borrowed
retired entries, oversized admission, rewrites, partial lines, replacement,
gzip ordering, callback timing and failed rebuilds. None of these establish
runtime acceptance or a deployment recommendation by themselves.
