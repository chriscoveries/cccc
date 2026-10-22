# actor-memory fixture

A real `/proc` capture, used by `crates/cccc-daemon/src/ops/actor_memory.rs`.

Captured 2026-10-22 from `agent`'s own processes on a development host.
Nothing here is synthetic and no value was edited afterwards.

## The tree

`2793514` is a live harness launcher (`node /usr/local/bin/codex …`) with four
descendants: `2793524`, `2793848`, `2794541`, `2806563`. Five processes in all.
`517722` is an unrelated real process captured from the same `/proc`, with no
children, included so a test can prove the walk never sums a process outside
the tree.

## What was kept

Only what the sampler reads, per process:

- `stat` — whole line, as the kernel writes it; the parent id (field 4) and the
  command name live here.
- `status` — `Name:` and `VmRSS:` only.
- `smaps_rollup` — `Pss:` only.

Every other line (uids, other counters, mapping detail) was dropped.

## The values the tests pin

Summed over the five members only:

- `VmRSS` 344896 kB = 336 MiB
- `Pss` 139715 kB = 136 MiB
- 5 processes

The unrelated process on its own is 126468 kB = 123 MiB, so a total of 459 MiB
would mean it was wrongly included.
