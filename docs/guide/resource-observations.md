# Resource observations

The local group header shows host RAM used and available; actor headers show
resident memory and CPU for the actor's observed process tree. These are
Linux-only, best-effort observations. On other systems, for remote sessions,
or when a process cannot be identified or read, a value is unavailable.
Managed Claude Agent View jobs do not currently expose their worker PID to the
daemon, so they are unavailable instead of being measured as terminal viewers.

The browser asks for a fresh observation about every ten seconds while a local
group is open and the tab is visible; a hidden tab stops asking until it is
visible again, and each client keeps at most one request in flight. The daemon
reads bounded `/proc` metadata only when explicitly requested, and all clients
of one daemon are served from a single fresh scan per five-second window:
later requests reuse the recent result — including a recorded unavailability —
instead of starting another full-host read. It uses the runtime-owned root and
rechecks its kernel start identity. It does not retain or return environment
or command-line contents. Memory is summed RSS, so shared pages can be counted
more than once; overlapping actor trees can also overlap. Host used memory is
MemTotal minus MemAvailable; available memory includes reclaimable cache.

CPU is measured across two snapshots as a percentage of one core; it may exceed
100% for multiple busy cores. The first observation has no CPU value. Each
snapshot totals its live processes' own CPU ticks together with the ticks of
the children they have already waited for, so a child that starts and exits
between two snapshots is counted once through its reaper rather than missed,
while live descendants are never counted twice. If a previously observed
member leaves the tree, the interval is unavailable because aggregate waited
ticks cannot identify which process supplied them. Equality of the two
process sets alone is never treated as proof that nothing ran in between. Linux
process information can change during collection, so the displayed tree is an
observation, not a billing or ownership record.
