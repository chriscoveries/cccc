# Resource observations

The local group header shows host RAM used and available; actor headers show
resident memory and CPU for the actor's observed process tree. These are
Linux-only, best-effort observations. On other systems, for remote sessions,
or when a process cannot be identified or read, a value is unavailable.
Managed Claude Agent View jobs do not currently expose their worker PID to the
daemon, so they are unavailable instead of being measured as terminal viewers.

The browser asks for a fresh observation about every ten seconds while a local
group is open. The daemon reads bounded `/proc` metadata only when explicitly
requested. It uses the runtime-owned root and rechecks its kernel start identity; the
managed DeepSeek fallback also checks launch attribution. It does not retain or
return environment or command-line contents. Memory is summed RSS,
so shared pages can be counted more than once; overlapping actor trees can also
overlap. Host used memory is MemTotal
minus MemAvailable; available memory includes reclaimable cache.

CPU is measured across two snapshots as a percentage of one core; it may exceed
100% for multiple busy cores. The first observation has no CPU value. When a
process is added or exits between snapshots, or a reading cannot be verified,
CPU is unavailable rather than a misleading zero. Linux process information
can change during collection, so the displayed tree is an observation, not a
billing or ownership record.
