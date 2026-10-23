# Captured lane-census stream (real RAM by harness)

Three consecutive real ticks of the lane census, captured 2026-10-22
21:21Z–21:30Z, used as the sample stream for the harness-RAM 72-hour aggregate
(`crates/cccc-daemon/src/ops/actor_memory_trend.rs`).

## Where it comes from

A census job samples every lane's process tree on a five-minute timer and folds
each pass into one small state file: one row per lane with the runtime it runs,
the last and peak proportional memory (`PSS`) it held, and how many samples it
has contributed. The capture behind this fixture read that state after each tick
and wrote one line per tick:

```json
{"captured_at": "...", "census_samples": 6895, "source_ts": "...",
 "rows": [{"harness": "hermes", "sample": 3, "pss_kb": 47102}, ...]}
```

`captured_at` is when this fixture read the state; `source_ts` is the census's
own `last_sample`, so a line is a real tick and not a re-read of the previous
one.

## What was kept, what was dropped

Kept: the harness id (`runtime`), the per-lane `pss_kb_last`, and a stable
per-harness index in place of the lane name. Dropped: lane and actor identities,
identities, group ids, swap, idle duty, sample counts and timestamps of first
sight, and every host-specific path and name. The test needs the harness and the
number, and nothing else.

## Why PSS and not RSS

The census state keeps `pss_kb_last` (proportional) and not an RSS figure, so
this stream pins the PSS side of the pair. The RSS side is asserted on the
captured `/proc` tree in `../actor-memory/` (336 MiB RSS / 136 MiB PSS, five
processes) — the aggregate's arithmetic is the same over both columns, which is
what the bucket tests in the module cover.

## What the test asserts

The test builds the report from these lines and checks it against the numbers
the capture itself contains: 48 readings of `hermes` across 16 lanes and three
ticks, peak 856 MiB, mean 212 MiB, plus the ordering (busiest key first) and
that a key the capture never saw is absent rather than zero. Nothing in the test
hard-codes a figure the capture does not already hold.
