# Host and Group health fixtures

Captured from this host on 2026-10-22T22:04:48Z, read-only, while the daemon was
running normally. Nothing here is synthetic: every number is what `/proc` said.

## The process tree

`root_pid` 2789094 is the cccc daemon and the 162 processes descended from it —
the whole real tree, 163 processes in each snapshot, read 6.027 s apart.

| file            | contents                                                     |
| --------------- | ------------------------------------------------------------ |
| `stat-t1.tsv`   | first reading: `pid<TAB>ppid<TAB>utime<TAB>stime` per process |
| `stat-t2.tsv`   | the same, 6.027 s later                                       |
| `proc-stat-2789094` | one raw `/proc/<pid>/stat` line, kept so the parser is tested against the kernel's real format (with its parenthesised, space-bearing command name) |

Summed `utime + stime`:

    t1  1236962 ticks
    t2  1237785 ticks
        +823 ticks over 6.027 s

At 100 clock ticks per second that is 8.23 s of CPU in 6.027 s of wall clock —
**136.6% of one core** across the tree, which is what a busy daemon looks like
(many threads on 16 CPUs). The percentage is per core, as `top` shows it, so it
is not divided by the CPU count.

## The host

| file       | what it holds                                                     |
| ---------- | ----------------------------------------------------------------- |
| `meminfo`  | the full real `/proc/meminfo`; `MemTotal` 32089156 kB, `MemAvailable` 19481288 kB |
| `loadavg`  | `8.17 5.76 5.47 5/3511 1774388`                                    |
| `stat`     | the full real `/proc/stat`; 16 `cpuN` lines = 16 online CPUs       |

`MemAvailable` is used rather than `MemFree` on purpose: on this host `MemFree`
was 2.1 GB while 19.5 GB was reclaimable, and showing the former would tell an
Operator the box was dying when it was not.

## What was dropped

Nothing but the lane and process names: the fixtures keep pids, times, memory
and load because the arithmetic needs them, and the shapes are the kernel's.
