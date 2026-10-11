# PTY history allocation retention after the ledger-index fix

The next measured retainer is `cccc-runtime::OutputBuffer`, beneath
`SessionHistory::push_with_terminal_responses`. Its payload limit did not bound
allocation overhead: every PTY read created another `Vec<u8>`, including reads
of only a few bytes. Millions of vector headers and small allocator blocks could
remain live even while the retained transcript stayed within its byte budget.

The bounded implementation reduces median RSS in the matched 18-actor, two-hour
claw reproduction from **813.74 MiB to 47.08 MiB (94.21% lower)**. Both
history windows are saturated. The independent code review passes, with zero
divergence in 67,000 differential sequences.

The base is `c909183d2338abe2210438d2a8c57c416882359e` on `devin/ram-index`,
which includes the ledger-index fix `8e1d544d`. This experiment investigates
chatty PTY history independently of the large-ledger workload in
[ram-survey.md](ram-survey.md). It does not measure the effect of that index fix
or establish that PTY output explains all production RSS growth.

## Fix

`1feeec2b3c1b2be56cc40f063a5f1f931741b748` coalesces incoming bytes into
blocks of at most 4 KiB. Partial eviction advances an offset in the first block,
so tiny writes neither allocate one vector each nor repeatedly move a full
history buffer. The configured byte window remains unchanged. Cursor positions,
raw bytes, replay, and UTF-8 handling retain their existing contracts.

An oversized write now allocates only its retained suffix. Terminal-mode probing
scans the full input but retains only a bounded suffix, including when the mode
escape occurs in discarded bytes or straddles two writes. `clear` and `trim_to`
release the old deque allocation.

## Reproduction

All builds and experiments run on **claw**. Production daemons on firestar and
MACDAD are untouched. The development machine is shared, so emitter throughput
varies with load; compare saturated history windows for the long-run result and
disclose emitted bytes for short profiles.
The two plain soaks overlap on this host, starting about ten minutes apart;
their final windows compare equal process ages rather than identical wall-clock
intervals. The independent RSS/swap monitor exposes paging during those windows.

The runtime harness is `crates/cccc-runtime/examples/pty_ram_soak.rs`. It starts
18 real PTY sessions, each running a finite Python process that writes `x\n`
every millisecond. PTY newline translation produces `x\r\n`. Each actor has a
separate transcript archive directory. The harness samples process RSS, absolute
output cursors, and terminal snapshots every ten seconds.
The completed comparison uses `baseline-soak-v3.csv` and `candidate-soak.csv`.
Earlier long runs with a shared archive directory hit quota errors and are
excluded; the reported runs use one directory per actor.

Both sides use 80 columns, 24 rows, a **2,000,000-byte hot history per actor**,
and a persistent 10 MiB archive per actor. This hot-history setting differs from
the daemon's default **10 MiB per actor**. The lower harness cap permits the
allocation limit and eviction behavior to be exercised during the same run.

Build from firestar through the development helper:

```sh
/home/agent/bin/claw-cargo WORKTREE build -p cccc-pair-runtime --release \
  --example pty_ram_soak --config profile.release.package.cccc-pair-runtime.debug=1
```

For the baseline, use the base commit plus the same harness file from the
candidate; do not copy the changed `output.rs`. On claw, copy each resulting
binary before syncing another version into its build tree. Run each binary in
a fresh case directory:

```sh
PTY_BINARY 7200 18 2 1 80 24 CASE_DIRECTORY > CASE.csv
heaptrack -o PROFILE PTY_BINARY 120 18 2 1 80 24 PROFILE_CASE > PROFILE.log 2>&1
heaptrack_print -f PROFILE.zst -l 1 -a 0 -T 0 -n 6 -s 2
heaptrack_print -f PROFILE.zst -F live.stacks --flamegraph-cost-type leaked \
  -p 0 -a 0 -T 0 -l 0
```

The finite emitters and owned-process guard stop the isolated actors on exit.
Static runtime session state stays live at process exit, allowing heaptrack's
live allocation stacks to identify retainers. These are live allocations at
capture, rather than a claim that they are unreachable leaks.

`scripts/pty_ram_daemon.py` exercises the same workload through a complete
daemon: six groups with three custom PTY actors each, a fresh synthetic
`CCCC_HOME`, and terminal-history reads through Unix RPC. It uses the daemon's
default history settings. Usage on claw:

```sh
python3 scripts/pty_ram_daemon.py CCCC_BINARY NEW_CASE_DIRECTORY 120
HEAPTRACK=1 python3 scripts/pty_ram_daemon.py CCCC_BINARY NEW_PROFILE_DIRECTORY 120
heaptrack_print -f NEW_PROFILE_DIRECTORY/heap.zst \
  -F NEW_PROFILE_DIRECTORY/peak.stacks --flamegraph-cost-type peak \
  -p 1 -a 0 -T 0 -l 0 -n 6 -s 2
```

When switching source versions in a reused build directory, refresh changed
source timestamps or clean the runtime package before building. A preserved
older timestamp can otherwise make Cargo reuse a stale runtime artifact.
The candidate daemon build explicitly recompiles `cccc-pair-runtime`; each
completed binary is copied and hashed before another source sync.

## Validation

The baseline fails both allocation regressions before the fix:

| Input | Retained payload budget | Baseline allocation | Required bound |
| --- | ---: | ---: | ---: |
| 200,000 one-byte writes | 128 KiB | 6,422,528 B | 140 KiB |
| One 1 MiB write | 1 KiB | 2,097,248 B | 12 KiB |

The tests count vector capacities and deque headers; the second also counts
terminal-mode probe capacity. They verify the bytes and absolute cursors as
well as the storage bound. Additional cases exercise partial eviction across
4 KiB boundaries, trimming, clear, non-UTF-8 bytes, and split terminal modes.

At `df313b43208c386ad6f52a8b7326c4faf643f5d2`, all **158 runtime unit tests
and 6 integration tests pass** on claw. Strict clippy passes for all runtime
targets. The changed Rust files pass `rustfmt --check`; `git diff --check` is
clean. Repository-wide formatting reports **36 unchanged hunks** outside these
files, identical in the frozen baseline and candidate. The Python controller
passes syntax checking.

Independent reviewer **lab-hermes2** returned **PASS** for `df313b43` after
67,000 randomized differential sequences: 60,000 mode-probe cases, 4,000
cursor/eviction cases, and 3,000 trim/clear cases, with zero divergence.
The reviewer's final full run passed all **163 unit tests**, including five
independently written boundary probes. Two process-reaping tests failed in an
earlier run; both passed in the final full run and in a focused rerun of all
32 process-tree tests with `--test-threads=1`. Reviewer probe sources live only
in the separate test worktree and are not part of this patch.

## Measurements and remaining retainers

The matched 120-second harness profiles retain the following allocations at
capture (decimal MB in the summary; exact bytes below):

| Live allocation family | Baseline | Candidate |
| --- | ---: | ---: |
| Output deque headers | 28,311,552 B | 27,648 B |
| Output payload and mode probe | 3,203,271 B | 2,695,744 B |
| Terminal mirror | 6,840,072 B | 6,839,304 B |
| Other runtime allocations | 150,669 B | 150,741 B |
| **Total live heap** | **38,505,564 B** | **9,713,437 B** |
| Peak heap | 38.81 MB | 9.98 MB |
| Peak RSS, including heaptrack overhead | 71.60 MB | 26.07 MB |

At the 120-second sample, the baseline emits 3,192,021 bytes and the candidate
emits 2,654,080 bytes. Both use the same emitter settings and harness, but these
are not equal-byte profiles because the shared machine changes their throughput.
Children may emit a few more bytes during process cleanup. The output
payload difference is expected from that throughput difference. The vector
header reduction and the allocation-bound regressions identify the mechanism
directly. The plain two-hour runs provide the saturated-window comparison.

The category table sums heaptrack's live flamegraph stacks once each:
`OutputBuffer` stacks containing `VecDeque` are headers; remaining
`OutputBuffer` stacks are payload/probe; `retach` stacks are the mirror; the
remainder is other. This groups allocation families rather than adding nested
stack totals.

The complete daemon profiles use default history settings, with 18 actors in
six groups and a small ledger. The following families are measured at each
daemon's global heap peak, rather than at process exit:

| Peak allocation family | Baseline | Candidate |
| --- | ---: | ---: |
| Output deque headers | 14,155,776 B | 55,296 B |
| Output payload and mode probe | 3,202,604 B | 5,562,944 B |
| Terminal mirror | 10,093,080 B | 10,094,040 B |
| Other daemon allocations | 1,420,328 B | 591,977 B |
| **Total peak heap** | **28,871,788 B** | **16,304,257 B** |
| Peak RSS, including heaptrack overhead | 100.86 MB | 73.05 MB |
| Total profiled process runtime | 169.50 s | 124.67 s |
| Final sample, elapsed workload seconds | 122 | 120 |
| Final absolute output cursors | 3,310,231 B | 5,230,605 B |
| Final ledger size | 33,542 B | 29,900 B |

The candidate emits about 58% more output during the daemon sample. Its larger
payload allocation reflects that throughput; despite it, deque allocation
falls from 13.5 MiB to 54 KiB. The daemon's terminal geometry differs from the
80-by-24 runtime harness, explaining its approximately 10.09 MB mirror. Neither
profile exercises a large message ledger or a persistent browser client.
The workload timer starts after all groups are started; the longer baseline
profile includes more setup time. Thus these profiles compare the same workload
settings, with their differing elapsed times and output volumes disclosed.

The complete daemon binary SHA-256 hashes are
`e62f6a4024cc2f7885aed1150584f44ffdc1ef9f37a790ff8c03d036896fc7e9`
(baseline) and
`b01b47f40b040d614eaae290a188fc22cf313a3ccab43cd0d2649d6722b7bddf`
(candidate). The controller is identical on both sides:
`bede544d9c2d720bdd4e196111564d2631f6ee3a9799d6cdfc61df40d90a3a93`.

The long-run comparison uses plain processes, without heaptrack. It reports
the median and range of RSS samples in each run's last 900 seconds, along with
the final cursor count. Each actor must pass its 2,000,000-byte history cap
before these windows are compared. The CSV column `retained_sample_bytes` is
the one-byte history read requested by the sampler; it is not the retained
history size. `snapshot_bytes` records serialized terminal snapshots.

| Plain two-hour runtime soak | Baseline | Candidate |
| --- | ---: | ---: |
| Final elapsed seconds | 7,202 | 7,200 |
| Last 900 seconds: RSS samples | 84 | 90 |
| Last 900 seconds: median RSS | 813.74 MiB | 47.08 MiB |
| Last 900 seconds: RSS range | 763.17–819.99 MiB | 47.08–47.12 MiB |
| Final absolute output cursors | 231,878,454 B | 237,470,718 B |
| Actors exceeding 2 MB hot history | 18 / 18 | 18 / 18 |
| Final serialized terminal snapshots | 157,572 B | 157,572 B |

All 18 actors exceeded their hot-history caps before the final sampling
windows. The committed archive check records over 10 MB per actor during the
final windows, and the supporting process watcher verifies all 18 emitters
remain alive throughout each window.

The runtime binaries are bound to these SHA-256 hashes:

| Artifact | SHA-256 |
| --- | --- |
| Baseline `output.rs` | `d2661e5409d631cbcfc54dc0e08e34c43b5bf1a1e8523e63c09f672465d09c2d` |
| Candidate `output.rs` | `4df3925806e9c7ac4f863d652835b398c6f3486935215b27d3b7d9837963ad71` |
| Shared `pty_ram_soak.rs` | `d1a03c540ead8a40894473ef919b45d53dab2aaa497366b7681c880613906420` |
| Baseline `baseline-pty-v3` | `1cf97d60cd7abd228a3a17ea1de9472a823d66d9d397a5d352787dff914d10fe` |
| Candidate `candidate-pty` | `b30b8fcc410c9c71adcae2945db3b67bcebbcc158594a68f8c7957b486eaa83d` |

The completed [measurement manifest](evidence/ram-second-retainer/results.json)
contains source and binary identities, CSV hashes, profile categories, and
validation results. [Long-run summary](evidence/ram-second-retainer/long-run-summary.json),
[baseline CSV](evidence/ram-second-retainer/baseline-soak-v3.csv),
[candidate CSV](evidence/ram-second-retainer/candidate-soak.csv), and
[RSS/swap observations](evidence/ram-second-retainer/soak-memory-watch.csv) are
committed as small text artifacts.
Raw artifacts are kept on claw under `/home/ghrunner/sol-b-ram-evidence`;
heaptrack archives and compiled binaries are not committed or pushed.

`scripts/pty_ram_watch.py` samples `/proc` RSS, swap, process age, and live Python
children for the isolated processes. Start it before the final 900-second
window, using the PIDs printed in the runtime CSVs:

```sh
python3 scripts/pty_ram_watch.py memory-watch.csv BASELINE_PID CANDIDATE_PID
python3 scripts/pty_ram_summary.py --watch memory-watch.csv \
  baseline-soak-v3.csv candidate-soak.csv > long-run-summary.json
```

Run the summary beside the case directories so it can inspect every actor's
archive. The runtime CSV's window uses elapsed harness time; the supporting
watcher's window uses process age from 6,300 to 7,200 seconds, differing by the
few seconds needed to start the actors. RSS plus swap is reported as a process
footprint, not as resident physical memory. This matters on the shared build
host: paging can lower RSS without releasing a retained allocation.

| Supporting final-window observations | Baseline | Candidate |
| --- | ---: | ---: |
| Samples | 90 | 90 |
| All 18 actors alive in every sample | yes | yes |
| Median swap | 0.46 MiB | 0.46 MiB |
| Swap range | 0.46–27.88 MiB | 0.46–0.46 MiB |
| Median RSS plus swap | 815.48 MiB | 47.54 MiB |
| RSS plus swap range | 791.10–820.47 MiB | 47.54–47.58 MiB |

The footprint median is computed from each sample's RSS plus swap, rather than
by adding the separate medians. The candidate's resident-memory range is only
40 KiB in its final window. The baseline retains substantially more memory
even after accounting for pages that the shared host swaps out.

The terminal mirror measured approximately 6.84 MB live across 18 actors in the
short profiles and stayed stable while output grew. After output allocation
overhead is bounded, the retained payload and terminal mirror become the largest
runtime allocations in this workload. It does not exercise long-lived web
clients or large-ledger delivery scans, so it cannot exclude retention or
allocator churn from those paths.
